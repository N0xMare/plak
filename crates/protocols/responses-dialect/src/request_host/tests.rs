use super::*;
use std::collections::BTreeMap;
use std::num::NonZeroU32;

#[test]
fn binding_mismatch_is_treated_as_not_this_request() {
    let binding = ResponsesRequestError::Replay(ResponsesReplayError::ArtifactBindingMismatch);
    assert!(binding.artifact_is_not_for_this_request());
    let canonical = ResponsesRequestError::Replay(ResponsesReplayError::ArtifactCanonicalMismatch);
    assert!(canonical.artifact_is_not_for_this_request());
    let expired = ResponsesRequestError::Replay(ResponsesReplayError::ArtifactExpired);
    assert!(expired.artifact_is_not_for_this_request());
    let coords = ResponsesRequestError::Replay(ResponsesReplayError::InvalidOutputCoordinates);
    assert!(coords.artifact_is_not_for_this_request());
    let prefix = ResponsesRequestError::Replay(ResponsesReplayError::ArtifactStablePrefixMismatch);
    assert!(prefix.artifact_is_not_for_this_request());
    let foreign = ResponsesRequestError::Replay(ResponsesReplayError::ArtifactTargetMismatch {
        axis: "model",
    });
    assert!(!foreign.artifact_is_not_for_this_request());
}

use plakous_model::{
    AttemptId, BranchId, CallId, CanonicalHistory, Capability, CapabilityConstraints,
    CapabilityRequirement, CapabilitySet, CompactionBlock, ConfiguredTool,
    ContextAdmissionRequirement, ConversationView, CredentialSelectionId, DataClassification,
    Digest, DigestDomain, EntryId, FactProvenance, FactSource, FeatureSupport, GenerationLimits,
    HistorySegment, HistorySegmentKind, InstructionBlock, InstructionSource,
    LocalDurabilityDisposition, ModelAdapterProfileId, ModelCacheAffinity, NamespaceId,
    NativeOptions, OpaqueBlock, OpaqueFallback, OutputPolicy, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
    ProviderSemanticEvidence, ProviderServiceTierId, ReasoningBlock, ReasoningVisibility,
    ReplayKindId, ReplayScope, RequestMetadata, ResolvedModelDescriptor, RouteId, RunId,
    RuntimeModelAuthority, SessionId, SessionWorkflowRootId, Support, TextBlock, TextPhase,
    ToolCallBlock, ToolCallState, ToolCallerDisposition, ToolCallerLineage, ToolConcurrency,
    ToolCorrelation, ToolEffect, ToolExecutionDisposition, ToolInputContract, ToolOutputContract,
    ToolResultBlock, ToolResultKind, TurnId, ValidationMode, WorkingProjection,
};
use plakous_responses::{
    ResponsesArtifactCodec, ResponsesArtifactProduction, ResponsesArtifactReplayKind,
    ResponsesContinuation, ResponsesReplayContract, ResponsesReplayItem,
    ResponsesSemanticRevisions,
};
use uuid::Uuid;

use crate::project_host::{
    ResponsesProviderRequestBinding, ResponsesReplayProjection, project_terminal_response,
};
use crate::response_host::{ResponsesResponse, ResponsesResponseLimits, ResponsesUsagePolicy};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn hosted_tool_and_allowed_caller_wire_shapes_are_exact() -> TestResult {
    assert_eq!(
        lower_hosted_tool(HostedTool::ProgrammaticComposition),
        json!({"type":"programmatic_tool_calling"})
    );
    assert_eq!(
        lower_hosted_tool(HostedTool::CodeInterpreter),
        json!({"type":"code_interpreter"})
    );
    for (caller, expected) in [
        (
            ToolCallerDisposition::DirectOnly,
            json!({"type":"function"}),
        ),
        (
            ToolCallerDisposition::ProviderProgrammaticOnly,
            json!({"type":"function","allowed_callers":["programmatic"]}),
        ),
        (
            ToolCallerDisposition::DirectAndProviderProgrammatic,
            json!({"type":"function","allowed_callers":["direct","programmatic"]}),
        ),
    ] {
        let mut wire = json!({"type":"function"});
        lower_allowed_callers(&mut wire, caller)?;
        assert_eq!(wire, expected);
    }
    Ok(())
}

#[test]
fn local_programmatic_lineage_is_rejected_at_provider_wire_boundary() -> TestResult {
    let lineage = ToolCallerLineage::local_programmatic(
        1,
        1,
        "cell-1",
        1,
        plakous_model::CallId::try_from(Uuid::from_u128(1))?,
        1,
    )?;
    let mut wire = json!({"type":"function"});
    let error = lower_program_caller(&mut wire, &lineage).expect_err("local lineage lowered");
    assert!(error.to_string().contains("provider-hosted"));
    assert_eq!(wire, json!({"type":"function"}));
    Ok(())
}

fn id<T>(value: u128) -> Result<T, plakous_model::IdentityError>
where
    T: TryFrom<Uuid, Error = plakous_model::IdentityError>,
{
    T::try_from(Uuid::from_u128(value))
}

fn metadata() -> Result<RequestMetadata, plakous_model::IdentityError> {
    Ok(RequestMetadata::for_turn(
        id::<RunId>(1)?,
        id::<TurnId>(2)?,
        id::<AttemptId>(3)?,
        Timestamp::new(1_700_000_000, 0)?,
    ))
}

fn workflow_metadata() -> Result<RequestMetadata, plakous_model::IdentityError> {
    Ok(RequestMetadata::for_session_workflow(
        id::<RunId>(1)?,
        SessionWorkflowRootId::new([3; 32])?,
        id::<AttemptId>(3)?,
        Timestamp::new(1_700_000_000, 0)?,
    ))
}

#[test]
fn openai_codex_session_workflow_emits_no_native_compaction_contract() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(21, "compact this")?])?,
        workflow_metadata()?,
    )
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    let input = lowered.body()["input"].as_array().ok_or("input")?;
    assert!(
        input
            .iter()
            .all(|item| item["type"] != "compaction_trigger")
    );
    assert_eq!(lowered.body().get("client_metadata"), None);
    Ok(())
}

#[test]
fn openai_codex_native_compaction_trigger_emits_compaction_trigger_item() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let namespace = NamespaceId::new("plakous.compaction")?;
    let native = NativeOptions::new(
        [(
            namespace,
            Value::String("native_responses_trigger_v1".into()),
        )]
        .into_iter()
        .collect(),
    );
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(22, "compact this")?])?,
        workflow_metadata()?,
    )
    .native(native)
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    let input = lowered.body()["input"].as_array().ok_or("input")?;
    assert_eq!(
        input.last().ok_or("last")?["type"].as_str(),
        Some("compaction_trigger")
    );
    let without = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(22, "compact this")?])?,
        workflow_metadata()?,
    )
    .build()?;
    assert_ne!(request.identity(), without.identity());
    Ok(())
}

#[test]
fn openai_codex_unknown_native_options_are_rejected() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let namespace = NamespaceId::new("plakous.unknown")?;
    let native = NativeOptions::new(
        [(namespace, Value::String("x".into()))]
            .into_iter()
            .collect(),
    );
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(23, "no")?])?,
        workflow_metadata()?,
    )
    .native(native)
    .build()?;
    let error = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )
    .expect_err("unknown native options");
    assert!(matches!(
        error,
        ResponsesRequestError::Option(ResponsesRequestOptionError::NativeOptions)
    ));
    Ok(())
}

#[test]
fn codex_service_tier_is_exact_and_orthogonal_to_reasoning() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let policy = ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content");
    let standard = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(31, "standard")?])?,
        metadata()?,
    )
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::High),
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let priority = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(32, "priority")?])?,
        metadata()?,
    )
    .processing_tier(ProcessingTierDisposition::ProviderNamed(
        ProviderServiceTierId::new("priority")?,
    ))
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Low),
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let standard = lower_responses_request(
        &standard,
        ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
    )?;
    let priority = lower_responses_request(
        &priority,
        ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
    )?;
    assert_eq!(standard.body().get("service_tier"), None);
    assert_eq!(standard.body()["reasoning"]["effort"], "high");
    assert_eq!(priority.body()["service_tier"], "priority");
    assert_eq!(priority.body()["reasoning"]["effort"], "low");
    Ok(())
}

#[test]
fn non_codex_responses_policy_rejects_named_service_tier() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(33, "unsupported")?])?,
        metadata()?,
    )
    .processing_tier(ProcessingTierDisposition::ProviderNamed(
        ProviderServiceTierId::new("priority")?,
    ))
    .build()?;
    assert!(
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(
                    ResponsesStoragePolicy::Stateless,
                    false,
                    false,
                ),
                None,
            ),
        )
        .is_err()
    );
    Ok(())
}

fn native_compaction_prior(
    summary: &str,
    native: &str,
) -> Result<ConversationItem, Box<dyn std::error::Error>> {
    Ok(ConversationItem::new(
        id::<EntryId>(22)?,
        Actor::Assistant,
        vec![ContentBlock::Compaction(
            CompactionBlock::new(summary)?.with_native_item(native.to_owned()),
        )],
        Provenance::ModelOutput,
    )?)
}

#[test]
fn requested_calls_with_results_prepare_fresh_on_shared_responses_routes() -> TestResult {
    let (protocol, family, model) = target_values()?;
    for owner in [
        ToolExecutionOwner::Local,
        ToolExecutionOwner::Mcp {
            server: plakous_model::McpServerId::new("fixture")?,
        },
    ] {
        for (raw, expected) in [
            (r#"{"q":"portable"}"#, r#"{"q":"portable"}"#),
            (r#""cell source""#, "cell source"),
        ] {
            let state = ToolCallState::Requested;
            let call = ToolCallBlock::new(
                id::<CallId>(25)?,
                ToolName::new("lookup")?,
                owner.clone(),
                state,
                ToolCorrelation::new(None, None, ToolCallerLineage::Direct)?,
                raw,
            )?;
            let items = vec![
                user(24, "look up")?,
                ConversationItem::new(
                    id::<EntryId>(25)?,
                    Actor::Assistant,
                    vec![
                        ContentBlock::ToolCall(call.clone()),
                        native_output(&protocol, &family, &model, "call-native")?,
                    ],
                    Provenance::ModelOutput,
                )?,
                ConversationItem::new(
                    id::<EntryId>(26)?,
                    Actor::Tool,
                    vec![ContentBlock::ToolResult(ToolResultBlock::new(
                        call.call_id(),
                        owner.clone(),
                        ToolResultKind::Success,
                        vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
                        ToolCorrelation::new(None, None, ToolCallerLineage::Direct)?,
                    )?)],
                    Provenance::ToolExecution,
                )?,
            ];
            let request = crate::test_support::model_request_builder(
                ConversationView::from_items(items)?,
                workflow_metadata()?,
            )
            .build()?;
            for policy in [
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                ResponsesRequestPolicy::supergrok_stateless(),
                ResponsesRequestPolicy::openrouter_stateless(),
            ] {
                let lowered = lower_responses_request(
                    &request,
                    ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
                )?;
                assert_eq!(lowered.body()["input"][1]["type"], "function_call");
                assert_eq!(lowered.body()["input"][1]["arguments"], expected);
                assert_eq!(lowered.body()["input"][2]["output"], "found");
            }
        }
    }
    Ok(())
}

#[test]
fn unbound_compaction_requires_exact_replay() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![native_compaction_prior(
            "opaque-ciphertext",
            r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#,
        )?])?,
        workflow_metadata()?,
    )
    .build()?;
    assert!(matches!(
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None
            )
        ),
        Err(ResponsesRequestError::Content(
            ResponsesRequestContentError::CompactionWithoutReplay
        ))
    ));
    Ok(())
}

fn compaction_binding() -> Result<plakous_model::CompactionReplayBinding, Box<dyn std::error::Error>>
{
    let (protocol, family, model) = target_values()?;
    Ok(plakous_model::CompactionReplayBinding::new(
        protocol,
        Some(family),
        model,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
    ))
}

fn bound_compaction_prior(
    summary: &str,
    native: &str,
) -> Result<ConversationItem, Box<dyn std::error::Error>> {
    let (protocol, family, model) = target_values()?;
    let replay = ReplayEnvelope::new(
        protocol,
        Some(family),
        ReplayScope::ExactAccountModel {
            model,
            account: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
        },
        ReplayKindId::new("responses.output_item")?,
        1,
        DataClassification::Sensitive,
        RawValue::from_string(native.to_owned())?,
    )?;
    Ok(ConversationItem::new(
        id::<EntryId>(22)?,
        Actor::Assistant,
        vec![
            ContentBlock::Compaction(
                CompactionBlock::new(summary)?
                    .with_native_item(native.to_owned())
                    .with_native_binding(compaction_binding()?),
            ),
            ContentBlock::Opaque(OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay)?),
        ],
        Provenance::ModelOutput,
    )?)
}

#[test]
fn projector_shaped_history_reopens_with_exact_artifact_and_prepares_fresh() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?;
    let make_replay = |native: Value| -> Result<ReplayEnvelope, Box<dyn std::error::Error>> {
        Ok(ReplayEnvelope::new(
            protocol.clone(),
            Some(family.clone()),
            ReplayScope::ExactAccountModel {
                model: model.clone(),
                account: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
            },
            ReplayKindId::new("responses.output_item")?,
            1,
            DataClassification::Sensitive,
            RawValue::from_string(native.to_string())?,
        )?)
    };
    let blank_native = json!({"id":"blank", "type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text","text":"","annotations":[]}]});
    let blank_replay = make_replay(blank_native.clone())?;
    let compact = bound_compaction_prior(
        "opaque-ciphertext",
        r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#,
    )?;
    let compact_replay = compact
        .blocks()
        .iter()
        .find_map(|block| match block {
            ContentBlock::Opaque(opaque) => Some(opaque.replay().clone()),
            _ => None,
        })
        .ok_or("compact replay")?;
    let call_native = json!({"id":"call-item","type":"function_call","status":"completed","call_id":"call-1","name":"lookup","arguments":"{}"});
    let call_replay = make_replay(call_native.clone())?;
    let call = ToolCallBlock::new(
        id::<CallId>(28)?,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(Some("call-1".to_owned()), None, ToolCallerLineage::Direct)?,
        "{}",
    )?;
    let producing = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(20, "first")?])?,
        metadata()?,
    )
    .tools(vec![tool("lookup", false)?])
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let items = vec![
        user(20, "first")?,
        ConversationItem::new(
            id::<EntryId>(21)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                blank_replay.clone(),
                OpaqueFallback::Omit,
            )?)],
            Provenance::ModelOutput,
        )?,
        compact,
        ConversationItem::new(
            id::<EntryId>(23)?,
            Actor::Assistant,
            vec![
                ContentBlock::ToolCall(call.clone()),
                ContentBlock::Opaque(OpaqueBlock::new(
                    call_replay.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?),
            ],
            Provenance::ModelOutput,
        )?,
        ConversationItem::new(
            id::<EntryId>(24)?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock::new(
                call.call_id(),
                call.owner().clone(),
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
                call.correlation().clone(),
            )?)],
            Provenance::ToolExecution,
        )?,
        user(25, "follow up")?,
    ];
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(items)?,
        metadata()?,
    )
    .tools(vec![tool("lookup", false)?])
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let now = Timestamp::new(1_700_000_000, 0)?;
    let artifact = output_artifact(
        &producing,
        &request,
        "responses-test-route",
        vec![blank_replay, compact_replay, call_replay],
        ResponsesStorageMode::Stateless,
        "fixture-response",
        None,
        now,
        b"history-reopen",
    )?;
    let reopened_items: Vec<ConversationItem> =
        serde_json::from_slice(&serde_json::to_vec(&request.input().items().to_vec())?)?;
    assert_eq!(reopened_items.as_slice(), request.input().items().to_vec());
    let artifact =
        ResponsesArtifactCodec::decode_inert(&ResponsesArtifactCodec::encode(&artifact)?)?;
    let active = active_artifact(&artifact)?;
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &active,
        &request,
        operation_authority(
            &request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        ResponsesReplayKind::new("responses.output_item", 1),
        None,
        None,
        now,
    )?;
    for policy in [
        ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
        ResponsesRequestPolicy::supergrok_stateless(),
        ResponsesRequestPolicy::openrouter_stateless(),
    ] {
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                policy,
                Some(bridge.context()),
            ),
        )?;
        assert_eq!(lowered.body()["input"][1], blank_native);
        assert_eq!(
            lowered.body()["input"][2]["encrypted_content"],
            "opaque-ciphertext"
        );
        let mut expected_call = call_native.clone();
        omit_output_only_input_fields(policy, &mut expected_call);
        assert_eq!(lowered.body()["input"][3], expected_call);
        let fresh = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None)
                .with_account_binding(&account),
        );
        if policy.authorizes_native_compaction_item_replay() {
            let fresh = fresh?;
            assert_eq!(fresh.body()["input"][1]["type"], "compaction");
            assert_eq!(fresh.body()["input"][2]["type"], "function_call");
            assert!(!fresh.body().to_string().contains("output_text"));
        } else {
            assert!(matches!(
                fresh,
                Err(ResponsesRequestError::Content(
                    ResponsesRequestContentError::CompactionWithoutReplay
                ))
            ));
        }
    }
    for config in [
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        )
        .with_account_binding(&Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"other-account"],
        )?),
    ] {
        let error =
            lower_responses_request(&request, config).expect_err("no foreign ciphertext replay");
        let classified = crate::ResponsesAttemptBuildError::Request(error).preparation_error_kind();
        assert_eq!(
            classified,
            plakous_model::ModelPreparationErrorKind::ProtectedHistory
        );
    }
    let other_protocol = ProtocolId::new("foreign.responses")?;
    let other_family = ModelFamilyId::new("foreign-family")?;
    let other_model = ModelId::new("foreign-model")?;
    for (dialect, target_family, target_model) in [
        (&other_protocol, &family, &model),
        (&protocol, &other_family, &model),
        (&protocol, &family, &other_model),
    ] {
        assert!(matches!(
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    dialect,
                    Some(target_family),
                    target_model,
                    ResponsesRequestPolicy::openai_codex(
                        false,
                        false,
                        "reasoning.encrypted_content"
                    ),
                    None,
                )
                .with_account_binding(&account)
            ),
            Err(ResponsesRequestError::Content(
                ResponsesRequestContentError::CompactionWithoutReplay
            ))
        ));
    }
    Ok(())
}

#[test]
fn openai_codex_replays_bound_native_compaction_item() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let prior = bound_compaction_prior(
        "opaque-ciphertext",
        r#"{"id":"compact_1","type":"compaction","encrypted_content":"opaque-ciphertext"}"#,
    )?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(21, "continue")?, prior])?,
        workflow_metadata()?,
    )
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        )
        .with_account_binding(&Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"responses-test-route"],
        )?),
    )?;
    let input = lowered.body()["input"].as_array().ok_or("input")?;
    let compaction = input
        .iter()
        .find(|item| item["type"] == "compaction")
        .ok_or("missing compaction item")?;
    assert_eq!(compaction["encrypted_content"], "opaque-ciphertext");
    assert!(compaction.get("id").is_none());
    Ok(())
}

#[test]
fn openai_codex_rejects_compaction_ciphertext_mismatch() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let prior = bound_compaction_prior(
        "semantic summary",
        r#"{"type":"compaction","encrypted_content":"opaque"}"#,
    )?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(21, "continue")?, prior])?,
        workflow_metadata()?,
    )
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        )
        .with_account_binding(&Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"responses-test-route"],
        )?),
    );
    assert!(matches!(
        lowered,
        Err(ResponsesRequestError::Content(
            ResponsesRequestContentError::CompactionWithoutReplay
        ))
    ));
    Ok(())
}

#[test]
fn non_codex_responses_policy_rejects_native_compaction_replay() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let prior = bound_compaction_prior(
        "opaque-ciphertext",
        r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#,
    )?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(21, "continue")?, prior])?,
        workflow_metadata()?,
    )
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::supergrok_stateless(),
            None,
        )
        .with_account_binding(&Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"responses-test-route"],
        )?),
    );
    assert!(matches!(
        lowered,
        Err(ResponsesRequestError::Content(
            ResponsesRequestContentError::CompactionWithoutReplay
        ))
    ));
    Ok(())
}

fn user(value: u128, text: &str) -> Result<ConversationItem, Box<dyn std::error::Error>> {
    Ok(ConversationItem::new(
        id::<EntryId>(value)?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(text, None)?)],
        Provenance::UserInput,
    )?)
}

fn native_output(
    protocol: &ProtocolId,
    family: &ModelFamilyId,
    model: &ModelId,
    marker: &str,
) -> Result<ContentBlock, Box<dyn std::error::Error>> {
    native_output_with_fallback(
        protocol,
        family,
        model,
        marker,
        OpaqueFallback::CompatibleReplay,
    )
}

fn native_output_with_fallback(
    protocol: &ProtocolId,
    family: &ModelFamilyId,
    model: &ModelId,
    marker: &str,
    fallback: OpaqueFallback,
) -> Result<ContentBlock, Box<dyn std::error::Error>> {
    let replay = ReplayEnvelope::new(
        protocol.clone(),
        Some(family.clone()),
        ReplayScope::ExactAccountModel {
            model: model.clone(),
            account: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
        },
        ReplayKindId::new("responses.output_item")?,
        1,
        DataClassification::Sensitive,
        RawValue::from_string(json!({"type":"message","marker":marker}).to_string())?,
    )?;
    Ok(ContentBlock::Opaque(OpaqueBlock::new(replay, fallback)?))
}

fn cross_provider_history(
    protocol: &ProtocolId,
    family: &ModelFamilyId,
    model: &ModelId,
    base: u128,
    include_reasoning: bool,
) -> Result<Vec<ConversationItem>, Box<dyn std::error::Error>> {
    let call = ToolCallBlock::new(
        id::<CallId>(base)?,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some(format!("call-{base}")),
            None,
            ToolCallerLineage::Direct,
        )?,
        r#"{"q":"portable"}"#,
    )?;
    let mut items = vec![ConversationItem::new(
        id::<EntryId>(base + 1)?,
        Actor::Assistant,
        vec![
            ContentBlock::Text(TextBlock::new("portable answer", Some(TextPhase::Final))?),
            native_output(protocol, family, model, "private-text-marker")?,
        ],
        Provenance::ModelOutput,
    )?];
    if include_reasoning {
        items.push(ConversationItem::new(
            id::<EntryId>(base + 2)?,
            Actor::Assistant,
            vec![
                ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Summary,
                    Some("portable reasoning summary".to_owned()),
                    None,
                )?),
                native_output(protocol, family, model, "private-reasoning-marker")?,
            ],
            Provenance::ModelOutput,
        )?);
    }
    items.extend([
        ConversationItem::new(
            id::<EntryId>(base + 3)?,
            Actor::Assistant,
            vec![
                ContentBlock::ToolCall(call.clone()),
                native_output(protocol, family, model, "private-tool-marker")?,
            ],
            Provenance::ModelOutput,
        )?,
        ConversationItem::new(
            id::<EntryId>(base + 4)?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock::new(
                call.call_id(),
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new(
                    "portable result",
                    None,
                )?)],
                call.correlation().clone(),
            )?)],
            Provenance::ToolExecution,
        )?,
        user(base + 5, "continue on the rebound target")?,
    ]);
    Ok(items)
}

fn tool(name: &str, custom: bool) -> Result<ConfiguredTool, Box<dyn std::error::Error>> {
    tool_with_concurrency(name, custom, ToolConcurrency::Parallel)
}

fn tool_with_concurrency(
    name: &str,
    custom: bool,
    concurrency: ToolConcurrency,
) -> Result<ConfiguredTool, Box<dyn std::error::Error>> {
    Ok(ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new(name)?,
            "1.0.0",
            format!("{name} description"),
            if custom {
                ToolInputContract::utf8(65_536)?
            } else {
                ToolInputContract::structured(json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"q": {"type": "string"}},
                    "required": ["q"],
                    "additionalProperties": false
                }))?
            },
            ToolOutputContract::portable_model_content(),
            concurrency,
            ToolEffect::ReadOnly,
        )?,
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    ))
}

#[test]
fn canonical_tools_carry_the_configured_mcp_owner() -> TestResult {
    let server = plakous_model::McpServerId::new("github")?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(10, "Use the appropriate tool.")?])?,
        metadata()?,
    )
    .tools(vec![
        tool("lookup", false)?,
        tool("mcp__github__create_issue", false)?.with_mcp_owner(server.clone())?,
    ])
    .build()?;
    assert_eq!(
        canonical_local_tools(&request),
        vec![
            ResponsesCanonicalTool::local_function(ToolName::new("lookup")?),
            ResponsesCanonicalTool::new(
                ToolName::new("mcp__github__create_issue")?,
                crate::ResponsesCanonicalToolKind::Function,
                crate::ResponsesCanonicalToolOwner::Mcp(server),
            ),
        ]
    );
    Ok(())
}

#[test]
fn provider_parallel_permission_is_independent_of_local_tool_scheduling() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(10, "Use the appropriate tool.")?])?,
        metadata()?,
    )
    .tools(vec![
        tool_with_concurrency("parallel_lookup", false, ToolConcurrency::Parallel)?,
        tool_with_concurrency("serial_write", false, ToolConcurrency::Serial)?,
    ])
    .build()?;

    for (admitted, expected) in [(true, true), (false, false)] {
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(
                    ResponsesStoragePolicy::ProviderStored,
                    false,
                    admitted,
                ),
                None,
            ),
        )?;
        assert_eq!(
            lowered.body().get("parallel_tool_calls"),
            Some(&Value::Bool(expected))
        );
    }
    Ok(())
}

#[test]
fn openrouter_tools_emit_documented_null_strict_and_serial_authority() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(11, "Use one tool.")?])?,
        metadata()?,
    )
    .tools(vec![tool_with_concurrency(
        "lookup",
        false,
        ToolConcurrency::Serial,
    )?])
    .limits(GenerationLimits::new(
        Some(NonZeroU32::new(64).expect("nonzero")),
        Vec::new(),
    )?)
    .build()?;
    let policy = ResponsesRequestPolicy::openrouter_stateless();
    validate_tools(
        request.tools(),
        request.tool_choice(),
        ParallelToolDisposition::Disabled,
        policy,
    )?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
    )?;
    assert_eq!(lowered.body()["tools"][0]["strict"], Value::Null);
    assert_eq!(lowered.body()["tool_choice"], "auto");
    assert!(lowered.body().get("parallel_tool_calls").is_none());
    assert!(lowered.body().get("include").is_none());
    Ok(())
}

fn target_values() -> Result<(ProtocolId, ModelFamilyId, ModelId), Box<dyn std::error::Error>> {
    Ok((
        ProtocolId::new("openai.responses")?,
        ModelFamilyId::new("gpt-family")?,
        ModelId::new("gpt-test")?,
    ))
}

#[test]
fn mixed_v2_projection_lowers_explicit_function_and_custom_presentations() -> TestResult {
    let base = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(90, "run it")?])?,
        metadata()?,
    )
    .tools(vec![
        tool("run_process", true)?,
        tool("poll_process", true)?,
    ])
    .tool_choice(ToolChoice::Specific(ToolName::new("run_process")?))
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;

    let codex_protocol = ProtocolId::new("openai.codex.responses")?;
    let codex_family = ModelFamilyId::new("responses-family")?;
    let codex_model = ModelId::new("gpt-5.3-codex-spark")?;
    let schema = plakous_model::PortableToolSchemaV2::new(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
        "properties":{"cmd":{"type":"string"},"yield_time_ms":{"type":"integer","minimum":0}},
        "required":["cmd"], "additionalProperties":false
    }))?;
    let output = plakous_model::PortableToolSchemaV2::new(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
        "properties":{"output":{"type":"string"}}, "required":["output"], "additionalProperties":false
    }))?;
    let source = Digest::sha256(DigestDomain::ToolDefinition, 2, &[b"test-projection"])?;
    let target = plakous_model::ToolProjectionTargetV2::new(
        plakous_model::IntegrationClass::Standard,
        codex_protocol.as_str(),
        "openai.codex_subscription",
        "openai.codex.compat.2026-07-31.r1",
        codex_family.as_str(),
        codex_model.as_str(),
        codex_model.as_str(),
        base.effective_provider_configuration_id(),
        1,
    )?;
    let projection = CompiledToolProjectionV2::new(
        target,
        source,
        source,
        vec![
            ProjectedToolDefinitionV2::new_custom(
                ToolName::new("run_process")?,
                vec![ToolName::new("run_process")?],
                ToolName::new("exec")?,
                "execute bounded JavaScript",
                plakous_model::Utf8TextContractV1::new(65_536)?,
                plakous_model::ProjectedCustomToolFormatV2::lark(
                    "start: source\nsource: /(?s).+/",
                )?,
                output.clone(),
                source,
            )?,
            ProjectedToolDefinitionV2::new(
                ToolName::new("poll_process")?,
                vec![ToolName::new("poll_process")?],
                ToolName::new("wait")?,
                "wait for an evaluation",
                schema,
                output,
                source,
            )?,
        ],
    )?;
    let prefix = base.stable_prefix().with_tool_projection(projection)?;
    let request = ModelRequest::from_parts(prefix, base.input().clone(), base.metadata())?;
    let codex = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &codex_protocol,
            Some(&codex_family),
            &codex_model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    let codex_tools = codex.body()["tools"].as_array().ok_or("codex tools")?;
    assert_eq!(codex_tools.len(), 2);
    assert_eq!(codex_tools[0]["name"], "exec");
    assert_eq!(codex_tools[0]["type"], "custom");
    assert_eq!(codex_tools[0]["format"]["type"], "grammar");
    assert_eq!(codex_tools[0]["format"]["syntax"], "lark");
    assert_eq!(
        codex_tools[0]["format"]["definition"],
        "start: source\nsource: /(?s).+/"
    );
    assert!(codex_tools[0].get("parameters").is_none());
    assert_eq!(codex_tools[1]["name"], "wait");
    assert_eq!(codex_tools[1]["type"], "function");
    assert_eq!(
        codex.custom_tools(),
        &BTreeSet::from([ToolName::new("exec")?])
    );
    assert_eq!(
        codex.body()["tool_choice"],
        json!({"type":"custom", "name":"exec"})
    );
    assert_eq!(
        codex_tools[1]["parameters"]["properties"]["yield_time_ms"]["type"],
        "integer"
    );
    assert_eq!(
        canonical_local_tools(&request),
        vec![
            ResponsesCanonicalTool::local_custom(ToolName::new("exec")?)
                .with_canonical_name(ToolName::new("run_process")?),
            ResponsesCanonicalTool::local_function(ToolName::new("wait")?)
                .with_canonical_name(ToolName::new("poll_process")?),
        ]
    );
    assert_eq!(codex.body()["parallel_tool_calls"], false);
    let drifted_model = ModelId::new("gpt-5.3-codex-neighbor")?;
    assert!(matches!(
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &codex_protocol,
                Some(&codex_family),
                &drifted_model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content",),
                None,
            ),
        ),
        Err(ResponsesRequestError::Option(
            ResponsesRequestOptionError::ProcessProjectionUnavailable
        ))
    ));
    Ok(())
}

#[test]
fn ordinary_tool_is_not_projected_by_provider_or_model_name() -> TestResult {
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(91, "run it")?])?,
        metadata()?,
    )
    .tools(vec![tool("run_process", false)?])
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let protocol = ProtocolId::new("openai.codex.responses")?;
    let family = ModelFamilyId::new("responses-family")?;
    let model = ModelId::new("gpt-codex-neighbor")?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    assert_eq!(lowered.body()["tools"][0]["name"], "run_process");
    Ok(())
}

fn harness_plan_identity() -> Result<Digest, plakous_model::DigestError> {
    Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"responses-test-tool-plan"],
    )
}

fn descriptor(route: &str) -> Result<ResolvedModelDescriptor, Box<dyn std::error::Error>> {
    let (protocol, family, default_model) = target_values()?;
    let model = if route == "responses-ptc-test-route" {
        ModelId::new("gpt-5.6-sol")?
    } else {
        default_model
    };
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(1_700_000_000, 0)?,
        None,
    );
    let mut capabilities = BTreeMap::new();
    for capability in [
        Capability::Streaming,
        Capability::Tools,
        Capability::Continuation,
        Capability::Caching,
    ] {
        capabilities.insert(
            capability,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
    }
    if route == "responses-ptc-test-route" {
        capabilities.insert(
            Capability::HostedProgrammaticToolCalling,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
    }
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        protocol,
        RouteId::new(route)?,
        model,
        None,
        Some(family),
        ModelAdapterProfileId::new("responses-test-adapter")?,
        CapabilitySet::new(capabilities, BTreeMap::new()),
    ))
}

fn selected_responses_configuration()
-> Result<SelectedProviderConfiguration, Box<dyn std::error::Error>> {
    let integration_profile = ModelAdapterProfileId::new("responses-test-adapter")?;
    let integration_revision = ProviderProfileRevisionId::new("responses-test-v1")?;
    Ok(SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            ProtocolId::new("openai.responses")?,
            RouteId::new("responses-test-route")?,
            integration_profile.clone(),
            integration_revision.clone(),
            ModelAdapterProfileId::new("responses-test-auth")?,
            ProviderProfileRevisionId::new("responses-test-auth-v1")?,
            ProviderAuthorityClassId::new("test-subscription")?,
            CredentialSelectionId::new("test-account")?,
            ModelId::new("gpt-test")?,
        ),
        ProviderBehaviorConfiguration {
            processing_tier: ProcessingTierDisposition::Standard,
            tool_mode: plakous_model::ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
            replay: ReplayDisposition::StrictEncrypted,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(NonZeroU32::MIN),
            storage: ProviderStorageDisposition::DisabledOnWire,
            retention: RetentionDisposition::RouteContract {
                profile: integration_profile,
                revision: integration_revision,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::Ephemeral,
        },
    )?)
}

fn configuration_authority(
    descriptor: &ResolvedModelDescriptor,
) -> Result<ResolvedProviderAuthorityBinding, Box<dyn std::error::Error>> {
    let runtime = RuntimeModelAuthority::new(
        descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-account"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"responses-test-credentials"],
        )?,
        1,
        Timestamp::new(1_700_000_000, 0)?,
        Some(Timestamp::new(1_700_000_100, 0)?),
    )?;
    Ok(ResolvedProviderAuthorityBinding::new(
        descriptor,
        &runtime,
        ProviderSemanticEvidence::new(1, b"responses-test-semantic-v1".to_vec())?,
    )?)
}

fn custom_descriptor(route: &str) -> Result<ResolvedModelDescriptor, Box<dyn std::error::Error>> {
    let standard = descriptor(route)?;
    Ok(ResolvedModelDescriptor::new_custom(
        standard.protocol().clone(),
        standard.route().clone(),
        standard.requested_model().clone(),
        standard.reported_model().cloned(),
        standard.model_family().cloned(),
        standard.profile().clone(),
        standard.capabilities().clone(),
    ))
}

#[test]
fn compilation_retains_exact_standard_and_custom_implementation_identity() -> TestResult {
    let selected = selected_responses_configuration()?;
    let standard = descriptor("responses-test-route")?;
    let expected_standard = ModelImplementationIdentity::release_owned(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        &standard,
        1,
    )?;
    let compiled_standard = compile_provider_configuration(
        &selected,
        standard.clone(),
        configuration_authority(&standard)?,
        ResponsesRequestPolicy::supergrok_stateless(),
    )?;
    assert_eq!(
        compiled_standard.effective().model_implementation(),
        &expected_standard
    );

    let custom = custom_descriptor("responses-test-route")?;
    let custom_implementation = ModelImplementationIdentity::custom(
        NamespaceId::new("example.responses.dynamic")?,
        7,
        NamespaceId::new("example.integration")?,
    )?;
    let compiled_custom = compile_provider_configuration_with_implementation(
        &selected,
        custom.clone(),
        custom_implementation.clone(),
        configuration_authority(&custom)?,
        ResponsesRequestPolicy::supergrok_stateless(),
    )?;
    assert_eq!(
        compiled_custom.effective().model_implementation(),
        &custom_implementation
    );
    let gateway = ResponsesRouterPolicy::gateway_managed(None, None)?;
    let routed_custom = compile_provider_configuration_with_implementation_and_router_policy(
        &selected,
        custom.clone(),
        custom_implementation.clone(),
        configuration_authority(&custom)?,
        ResponsesRequestPolicy::supergrok_stateless(),
        Some(&gateway),
    )?;
    assert_ne!(
        compiled_custom.effective_identity(),
        routed_custom.effective_identity()
    );
    let direct_policy: Value = serde_json::from_slice(
        compiled_custom
            .effective()
            .family_policy()
            .canonical_bytes(),
    )?;
    let routed_policy: Value =
        serde_json::from_slice(routed_custom.effective().family_policy().canonical_bytes())?;
    assert!(direct_policy.get("router_policy").is_none());
    assert_eq!(
        routed_policy["router_policy"],
        serde_json::to_value(gateway.execution_identity()?)?
    );

    let error = compile_provider_configuration_with_implementation(
        &selected,
        custom.clone(),
        expected_standard,
        configuration_authority(&custom)?,
        ResponsesRequestPolicy::supergrok_stateless(),
    )
    .expect_err("foreign descriptor/implementation class must fail");
    assert_eq!(
        error,
        ResponsesProviderConfigurationError::ProviderConfiguration(
            ProviderConfigurationError::ForeignIntegrationClass
        )
    );
    Ok(())
}

#[test]
fn selected_policy_mismatch_fails_on_one_field() -> TestResult {
    let mut selected = selected_responses_configuration()?;
    let mut behavior = selected.behavior().clone();
    behavior.replay = ReplayDisposition::Disabled;
    selected = SelectedProviderConfiguration::new(selected.route().clone(), behavior)?;
    let descriptor = descriptor("responses-test-route")?;
    let error = compile_provider_configuration(
        &selected,
        descriptor.clone(),
        configuration_authority(&descriptor)?,
        ResponsesRequestPolicy::supergrok_stateless(),
    )
    .expect_err("one changed replay field must fail");
    assert_eq!(
        error,
        ResponsesProviderConfigurationError::PolicyMismatch("replay")
    );
    Ok(())
}

#[test]
fn canonical_custom_tool_is_compilable_but_requires_request_projection() -> TestResult {
    let mut selected = selected_responses_configuration()?;
    let mut behavior = selected.behavior().clone();
    behavior.tools = vec![tool("canonical_process_start", true)?];
    selected = SelectedProviderConfiguration::new(selected.route().clone(), behavior)?;
    let descriptor = descriptor("responses-test-route")?;
    compile_provider_configuration(
        &selected,
        descriptor.clone(),
        configuration_authority(&descriptor)?,
        ResponsesRequestPolicy::supergrok_stateless(),
    )?;

    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(92, "run it")?])?,
        metadata()?,
    )
    .tools(vec![tool("canonical_process_start", true)?])
    .build()?;
    let (protocol, family, model) = target_values()?;
    assert!(matches!(
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::supergrok_stateless(),
                None,
            ),
        ),
        Err(ResponsesRequestError::Option(
            ResponsesRequestOptionError::CustomTools
        ))
    ));
    Ok(())
}

#[test]
fn family_policy_envelope_is_deterministic_canonical_and_roundtrips() -> TestResult {
    let selected = selected_responses_configuration()?;
    let descriptor = descriptor("responses-test-route")?;
    let policy = ResponsesRequestPolicy::supergrok_stateless();
    let first = compile_provider_configuration(
        &selected,
        descriptor.clone(),
        configuration_authority(&descriptor)?,
        policy,
    )?;
    let second = compile_provider_configuration(
        &selected,
        descriptor.clone(),
        configuration_authority(&descriptor)?,
        policy,
    )?;
    assert_eq!(first, second);
    let canonical: Value =
        serde_json::from_slice(first.effective().family_policy().canonical_bytes())?;
    assert_eq!(canonical["schema_revision"], 1);
    assert_eq!(canonical["compiler_revision"], 1);
    assert_eq!(
        canonical["execution_revision"],
        RESPONSES_REQUEST_POLICY_EXECUTION_REVISION
    );
    assert_eq!(canonical["store_field"], "emit");
    assert_eq!(canonical["reasoning_summary"], "concise");
    assert_eq!(canonical["reasoning_include"], "always");
    assert_eq!(canonical["replay"], "strict");
    let encoded = serde_json::to_vec(&first)?;
    let decoded: CompiledProviderConfiguration = serde_json::from_slice(&encoded)?;
    assert_eq!(decoded, first);
    validate_supported_family_policy(
        descriptor
            .model_family()
            .ok_or("descriptor omitted family")?,
        decoded.effective().family_policy(),
    )?;
    assert_eq!(
        validate_supported_family_policy(
            &ModelFamilyId::new("foreign-responses-family")?,
            decoded.effective().family_policy(),
        ),
        Err(ResponsesProviderConfigurationError::ProviderConfiguration(
            ProviderConfigurationError::ForeignTarget,
        ))
    );
    Ok(())
}

#[test]
fn family_policy_support_rejects_each_unknown_revision() -> TestResult {
    let selected = selected_responses_configuration()?;
    let descriptor = descriptor("responses-test-route")?;
    let compiled = compile_provider_configuration(
        &selected,
        descriptor.clone(),
        configuration_authority(&descriptor)?,
        ResponsesRequestPolicy::supergrok_stateless(),
    )?;
    let current = compiled.effective().family_policy();
    let unsupported = |schema, compiler, dialect, bytes| {
        FamilyPolicyEnvelope::new(current.family().clone(), schema, compiler, dialect, bytes)
    };
    for envelope in [
        unsupported(
            RESPONSES_FAMILY_POLICY_SCHEMA_REVISION + 1,
            RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
            RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
            current.canonical_bytes().to_vec(),
        )?,
        unsupported(
            RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
            RESPONSES_FAMILY_POLICY_COMPILER_REVISION + 1,
            RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
            current.canonical_bytes().to_vec(),
        )?,
        unsupported(
            RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
            RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
            RESPONSES_REQUEST_POLICY_EXECUTION_REVISION + 1,
            current.canonical_bytes().to_vec(),
        )?,
    ] {
        assert_eq!(
            validate_supported_family_policy(current.family(), &envelope),
            Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                ProviderConfigurationError::UnsupportedRevision,
            ))
        );
    }

    for field in [
        "schema_revision",
        "compiler_revision",
        "execution_revision",
        "artifact_schema_revision",
        "artifact_codec_revision",
    ] {
        let mut canonical: Value = serde_json::from_slice(current.canonical_bytes())?;
        canonical[field] = Value::from(99);
        let envelope = unsupported(
            RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
            RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
            RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
            serde_json::to_vec(&canonical)?,
        )?;
        assert_eq!(
            validate_supported_family_policy(current.family(), &envelope),
            Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                ProviderConfigurationError::UnsupportedRevision,
            ))
        );

        let mut defaulted: Value = serde_json::from_slice(current.canonical_bytes())?;
        defaulted[field] = Value::from(0);
        let envelope = unsupported(
            RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
            RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
            RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
            serde_json::to_vec(&defaulted)?,
        )?;
        assert!(matches!(
            validate_supported_family_policy(current.family(), &envelope),
            Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                ProviderConfigurationError::UnsupportedRevision
            ))
        ));

        let mut missing: Value = serde_json::from_slice(current.canonical_bytes())?;
        missing
            .as_object_mut()
            .ok_or("canonical policy fixture must be an object")?
            .remove(field);
        let envelope = unsupported(
            RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
            RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
            RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
            serde_json::to_vec(&missing)?,
        )?;
        assert!(matches!(
            validate_supported_family_policy(current.family(), &envelope),
            Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                ProviderConfigurationError::UnsupportedRevision
            ))
        ));
    }
    Ok(())
}

#[test]
fn platform_function_strictness_changes_execution_authority() -> TestResult {
    for policy in [
        ResponsesRequestPolicy::openai_platform(
            ResponsesStoragePolicy::ProviderStored,
            false,
            false,
        ),
        ResponsesRequestPolicy::openai_platform_stateless(),
    ] {
        assert_eq!(policy.function_strict, FunctionStrictPolicy::IncludeFalse);
        assert_eq!(
            policy.structured_output,
            StructuredOutputPolicy::StrictJsonSchema
        );
        let mut strict = policy;
        strict.function_strict = FunctionStrictPolicy::IncludeTrue;
        assert_ne!(policy.execution_identity()?, strict.execution_identity()?);
    }
    Ok(())
}

#[test]
fn policy_and_envelope_execution_identity_are_equal() -> TestResult {
    let selected = selected_responses_configuration()?;
    let descriptor = descriptor("responses-test-route")?;
    let policy = ResponsesRequestPolicy::supergrok_stateless();
    let compiled = compile_provider_configuration(
        &selected,
        descriptor.clone(),
        configuration_authority(&descriptor)?,
        policy,
    )?;
    let canonical: Value =
        serde_json::from_slice(compiled.effective().family_policy().canonical_bytes())?;
    assert_eq!(
        canonical["execution_identity"],
        serde_json::to_value(policy.execution_identity()?)?
    );
    Ok(())
}

fn operation_binding(
    request: &ModelRequest,
    route: &str,
) -> Result<ResponsesArtifactBinding, Box<dyn std::error::Error>> {
    let descriptor = descriptor(route)?;
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    Ok(ResponsesArtifactBinding::from_admitted_operation(
        &descriptor,
        &plan,
        request,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[route.as_bytes()])?,
        ResponsesSemanticRevisions::new(1, 1, 1)?,
        None,
    )?)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the fixture seals every independent artifact authority fact"
)]
fn output_artifact(
    producing_request: &ModelRequest,
    covered_request: &ModelRequest,
    route: &str,
    replay: Vec<ReplayEnvelope>,
    storage: ResponsesStorageMode,
    response_id: &str,
    expires_at: Option<Timestamp>,
    now: Timestamp,
    tag: &[u8],
) -> Result<ResponsesArtifact, Box<dyn std::error::Error>> {
    let descriptor = descriptor(route)?;
    let plan = descriptor.validate_request(producing_request.clone(), ValidationMode::Strict)?;
    let output_count = u32::try_from(replay.len())?;
    let producing_count = producing_request.input().items().len();
    let covered_count = producing_count
        .checked_add(replay.len())
        .ok_or_else(|| std::io::Error::other("artifact coverage overflow"))?;
    let covered_projection = covered_request
        .input()
        .canonical_item_prefix_identity(covered_count)?
        .identity();
    let physical_request = Digest::sha256(
        DigestDomain::ProviderOperation,
        1,
        &[b"request-host-fixture-physical", tag],
    )?;
    let terminal_response = Digest::sha256(
        DigestDomain::ProviderOperation,
        1,
        &[b"request-host-fixture-terminal", tag],
    )?;
    let production = ResponsesArtifactProduction::from_admitted_response(
        producing_request,
        &plan,
        physical_request,
        terminal_response,
        output_count,
        covered_projection,
    )?;
    let first = replay
        .first()
        .ok_or_else(|| std::io::Error::other("fixture requires output replay"))?;
    let replay_contract = ResponsesReplayContract::new(ResponsesArtifactReplayKind::new(
        first.kind().clone(),
        first.version(),
    )?);
    let replay = replay
        .into_iter()
        .enumerate()
        .map(|(index, envelope)| {
            Ok(ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                u32::try_from(index)?,
                envelope,
            ))
        })
        .collect::<Result<Vec<_>, std::num::TryFromIntError>>()?;
    let continuation = if storage == ResponsesStorageMode::ProviderStored {
        Some(ResponsesContinuation::new(
            response_id,
            production.covered_projection_items(),
            production.covered_projection(),
            production.terminal_response(),
        )?)
    } else {
        None
    };
    Ok(ResponsesArtifact::new(
        operation_binding(producing_request, route)?,
        production,
        replay_contract,
        replay,
        continuation,
        None,
        storage,
        expires_at,
        now,
    )?)
}

fn operation_authority(
    request: &ModelRequest,
    route: &str,
    storage: ResponsesStoragePolicy,
) -> Result<ResponsesRequestAuthority, Box<dyn std::error::Error>> {
    let descriptor = descriptor(route)?;
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    Ok(ResponsesRequestAuthority::from_admitted_operation(
        &descriptor,
        &plan,
        request,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[route.as_bytes()])?,
        ResponsesSemanticRevisions::new(1, 1, 1)?,
        None,
        storage,
    )?)
}

fn active_artifact(
    artifact: &ResponsesArtifact,
) -> Result<ActiveFamilyArtifact, Box<dyn std::error::Error>> {
    Ok(ActiveFamilyArtifact::new(
        "responses",
        RESPONSES_ARTIFACT_SCHEMA_REVISION,
        RESPONSES_ARTIFACT_CODEC_REVISION,
        artifact.binding().identity(),
        ResponsesArtifactCodec::encode(artifact)?,
    )?)
}

fn active_replay_fixture(
    storage: ResponsesStorageMode,
    expires_at: Option<Timestamp>,
) -> Result<(ModelRequest, ActiveFamilyArtifact), Box<dyn std::error::Error>> {
    active_replay_fixture_with_encrypted(
        storage,
        expires_at,
        Some("completed"),
        Some("fixture-encrypted"),
    )
}

fn active_replay_fixture_with_encrypted(
    storage: ResponsesStorageMode,
    expires_at: Option<Timestamp>,
    status: Option<&str>,
    encrypted_content: Option<&str>,
) -> Result<(ModelRequest, ActiveFamilyArtifact), Box<dyn std::error::Error>> {
    let (protocol, family, model) = target_values()?;
    let mut native = json!({
        "id": "fixture-reasoning",
        "type": "reasoning",
        "summary": []
    });
    if let Some(status) = status {
        native["status"] = Value::String(status.to_owned());
    }
    if let Some(encrypted_content) = encrypted_content {
        native["encrypted_content"] = Value::String(encrypted_content.to_owned());
    }
    let replay = ReplayEnvelope::new(
        protocol,
        Some(family),
        ReplayScope::ExactAccountModel {
            model,
            account: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
        },
        ReplayKindId::new("openai.output_item")?,
        2,
        DataClassification::Sensitive,
        RawValue::from_string(native.to_string())?,
    )?;
    let prefix = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![user(500, "fixture first")?],
    )?;
    let producing_request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![prefix.clone()])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let output = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            id::<EntryId>(501)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                replay.clone(),
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![
                prefix,
                output,
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![user(502, "fixture next")?],
                )?,
            ])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let artifact = output_artifact(
        &producing_request,
        &request,
        "responses-test-route",
        vec![replay],
        storage,
        "fixture-response",
        expires_at,
        Timestamp::new(1_700_000_000, 0)?,
        b"active-replay-fixture",
    )?;
    Ok((request, active_artifact(&artifact)?))
}

#[test]
fn strict_lowering_rejects_active_reasoning_without_encrypted_content() -> TestResult {
    let (request, active) = active_replay_fixture_with_encrypted(
        ResponsesStorageMode::Stateless,
        None,
        Some("completed"),
        None,
    )?;
    let (protocol, family, model) = target_values()?;
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &active,
        &request,
        operation_authority(
            &request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        ResponsesReplayKind::new("openai.output_item", 2),
        None,
        None,
        Timestamp::new(1_700_000_000, 0)?,
    )?;
    assert!(matches!(
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::supergrok_stateless(),
                Some(bridge.context()),
            ),
        ),
        Err(ResponsesRequestError::Replay(
            ResponsesReplayError::StrictEncryptedReasoning
        ))
    ));
    Ok(())
}

#[test]
fn strict_lowering_accepts_codex_reasoning_without_status() -> TestResult {
    let (request, active) = active_replay_fixture_with_encrypted(
        ResponsesStorageMode::Stateless,
        None,
        None,
        Some("fixture-encrypted"),
    )?;
    let (protocol, family, model) = target_values()?;
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &active,
        &request,
        operation_authority(
            &request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        ResponsesReplayKind::new("openai.output_item", 2),
        None,
        None,
        Timestamp::new(1_700_000_000, 0)?,
    )?;
    lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            Some(bridge.context()),
        ),
    )?;
    Ok(())
}

#[test]
fn active_context_replays_bound_output_and_portably_lowers_compatible_unbound_output() -> TestResult
{
    let (base, active) = active_replay_fixture(ResponsesStorageMode::Stateless, None)?;
    let (protocol, family, model) = target_values()?;
    let mut items = base.input().items().to_vec();
    items.insert(
        2,
        ConversationItem::new(
            id::<EntryId>(503)?,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new(
                    "portable compatible-but-unbound answer",
                    Some(TextPhase::Final),
                )?),
                native_output(
                    &protocol,
                    &family,
                    &model,
                    "compatible-unbound-native-marker",
                )?,
            ],
            Provenance::ModelOutput,
        )?,
    );
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(items)?,
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let authority = operation_authority(
        &request,
        "responses-test-route",
        ResponsesStoragePolicy::Stateless,
    )?;
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &active,
        &request,
        authority,
        ResponsesReplayKind::new("openai.output_item", 2),
        None,
        None,
        Timestamp::new(1_700_000_000, 0)?,
    )?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            Some(bridge.context()),
        ),
    )?;
    let wire = lowered.body().to_string();
    assert!(wire.contains("fixture-reasoning"));
    assert!(wire.contains("portable compatible-but-unbound answer"));
    assert!(!wire.contains("compatible-unbound-native-marker"));
    Ok(())
}

#[test]
fn nested_digest_versions_are_part_of_authority_and_prefix_identity() -> TestResult {
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(9, "exact identity")?])?,
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let binding = operation_binding(&request, "test.responses")?;
    let raw = [0x5a; 32];
    let request_v1 = Digest::from_sha256_bytes(DigestDomain::HistoryProjection, 1, raw)?;
    let request_v2 = Digest::from_sha256_bytes(DigestDomain::HistoryProjection, 2, raw)?;
    let plan = harness_plan_identity()?;
    assert_ne!(
        operation_authority_digest(
            &binding,
            ResponsesStoragePolicy::Stateless,
            request_v1,
            plan,
        )?,
        operation_authority_digest(
            &binding,
            ResponsesStoragePolicy::Stateless,
            request_v2,
            plan,
        )?
    );

    let authority_v1 = Digest::from_sha256_bytes(DigestDomain::ModelBinding, 1, raw)?;
    let authority_v2 = Digest::from_sha256_bytes(DigestDomain::ModelBinding, 2, raw)?;
    assert_ne!(
        prefix_binding_digest(authority_v1, request.input().identity())?,
        prefix_binding_digest(authority_v2, request.input().identity())?
    );
    Ok(())
}

#[test]
fn codex_instructions_are_an_identified_input_developer_message() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let build = |tools: Vec<_>| -> Result<_, Box<dyn std::error::Error>> {
        Ok(crate::test_support::model_request_builder(
            ConversationView::from_items(vec![user(10, "Find the answer.")?])?,
            metadata()?,
        )
        .instructions(vec![InstructionBlock::new(
            InstructionSource::Developer,
            "Be precise.",
        )?])
        .tools(tools)
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Medium),
            requirement: CapabilityRequirement::Required,
        })
        .build()?)
    };
    let affinity = Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"codex-instructions"])?;
    let lower = |request: &_, policy| {
        lower_responses_request(
            request,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None)
                .with_cache_affinity(Some(&affinity)),
        )
    };
    let codex = ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content");
    let with_tools = build(vec![tool("lookup", false)?])?;
    let ordinary = lower(&with_tools, codex)?;
    assert!(ordinary.body().get("instructions").is_none());
    assert_eq!(ordinary.body()["tools"][0]["name"], "lookup");
    let input = ordinary.body()["input"].as_array().ok_or("input")?;
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "developer");
    assert_eq!(input[0]["content"][0]["text"], "Be precise.");
    let message_id = input[0]["id"].as_str().ok_or("message id")?;
    assert_codex_item_uuid(message_id, "msg_")?;
    assert_eq!(input[1]["role"], "user");
    assert_eq!(
        lower(&with_tools, codex)?.body()["input"][0]["id"],
        input[0]["id"],
        "the instruction item identity is stable across retries"
    );

    let lite_policy = ResponsesRequestPolicy::openai_codex_lite("reasoning.encrypted_content");
    let lite = lower(&with_tools, lite_policy)?;
    let lite_input = lite.body()["input"].as_array().ok_or("lite input")?;
    assert_eq!(lite_input[0]["type"], "additional_tools");
    assert_codex_item_uuid(lite_input[0]["id"].as_str().ok_or("tool id")?, "at_")?;
    assert_eq!(lite_input[1]["role"], "developer");
    assert_eq!(lite_input[1]["id"], input[0]["id"]);
    let toolless = lower(&build(Vec::new())?, lite_policy)?;
    let toolless_input = toolless.body()["input"]
        .as_array()
        .ok_or("tool-less input")?;
    assert_eq!(toolless_input[0]["role"], "developer");
    assert!(
        toolless_input
            .iter()
            .all(|item| item["type"] != "additional_tools"),
        "a tool-less Lite request declares no additional tools"
    );
    let mut top_level = codex;
    top_level.instruction_placement = InstructionPlacementPolicy::TopLevelInstructions;
    assert_ne!(
        codex.execution_identity()?,
        top_level.execution_identity()?,
        "instruction placement binds the request policy identity"
    );
    Ok(())
}

fn assert_codex_item_uuid(id: &str, prefix: &str) -> TestResult {
    let suffix = id.strip_prefix(prefix).ok_or("item id prefix")?;
    let uuid = Uuid::parse_str(suffix)?;
    assert_eq!(uuid.get_version_num(), 8);
    assert_eq!(uuid.get_variant(), uuid::Variant::RFC4122);
    assert_eq!(suffix, uuid.hyphenated().to_string());
    Ok(())
}

#[test]
fn codex_client_item_ids_are_never_bare_hex_and_are_stable() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(10, "Find the answer.")?])?,
        metadata()?,
    )
    .instructions(vec![InstructionBlock::new(
        InstructionSource::Developer,
        "Be precise.",
    )?])
    .tools(vec![tool("lookup", false)?])
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Medium),
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let affinity = Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"codex-item-ids"])?;
    for policy in [
        ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
        ResponsesRequestPolicy::openai_codex_lite("reasoning.encrypted_content"),
    ] {
        let lower = || {
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None)
                    .with_cache_affinity(Some(&affinity)),
            )
        };
        let first = lower()?;
        assert_eq!(
            first.body(),
            lower()?.body(),
            "retry/reopen lowering is stable"
        );
        let input = first.body()["input"].as_array().ok_or("input")?;
        let mut count = 0;
        for item in input {
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            count += 1;
            let (prefix, suffix) = id.split_once('_').ok_or("item id separator")?;
            assert!(!suffix.bytes().all(|byte| byte.is_ascii_hexdigit()), "{id}");
            assert_codex_item_uuid(id, &format!("{prefix}_"))?;
        }
        assert_eq!(
            count,
            if policy.is_codex_responses_lite() {
                2
            } else {
                1
            }
        );
    }
    Ok(())
}

#[test]
fn platform_golden_preserves_ordered_request_shape() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(10, "Find the answer.")?])?,
        metadata()?,
    )
    .instructions(vec![
        InstructionBlock::new(InstructionSource::Developer, "Be precise.")?,
        InstructionBlock::new(InstructionSource::Harness, "Use tools when useful.")?,
    ])
    .tools(vec![tool("lookup", false)?])
    .limits(GenerationLimits::new(
        Some(NonZeroU32::new(512).expect("nonzero")),
        Vec::new(),
    )?)
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_platform(
                ResponsesStoragePolicy::ProviderStored,
                false,
                true,
            ),
            None,
        ),
    )?;
    assert_eq!(
        lowered.body(),
        &json!({
            "model": "gpt-test",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Find the answer."}]
            }],
            "stream": true,
            "store": true,
            "instructions": "Be precise.\n\nUse tools when useful.",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "lookup description",
                // The durable V2 document carries the `$schema` dialect URI;
                // the wire projection strips it (`wire_document`), so the
                // lowered tool object never spends those bytes.
                "parameters": {
                    "type": "object",
                    "properties": {"q": {"type": "string"}},
                    "required": ["q"],
                    "additionalProperties": false
                },
                "strict": false
            }],
            "parallel_tool_calls": true,
            "tool_choice": "auto",
            "max_output_tokens": 512
        })
    );
    assert_eq!(lowered.replay_mode(), &ResponsesReplayMode::Full);
    assert!(!format!("{lowered:?}").contains("Find the answer"));
    Ok(())
}

#[test]
fn assistant_history_uses_output_text_and_projects_commentary_without_replay() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let assistant = |phase| {
        ConversationItem::new(
            id::<EntryId>(15).expect("entry"),
            Actor::Assistant,
            vec![ContentBlock::Text(
                TextBlock::new("prior answer", phase).expect("text"),
            )],
            Provenance::ModelOutput,
        )
        .expect("assistant item")
    };
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![assistant(Some(TextPhase::Final))])?,
        metadata()?,
    )
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    assert_eq!(lowered.body()["input"][0]["role"], "assistant");
    assert_eq!(
        lowered.body()["input"][0]["content"][0]["type"],
        "output_text"
    );
    assert_eq!(
        lowered.body()["input"][0]["content"][0]["text"],
        "prior answer"
    );

    let commentary = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![assistant(Some(TextPhase::Commentary))])?,
        metadata()?,
    )
    .build()?;
    let lowered = lower_responses_request(
        &commentary,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    assert_eq!(lowered.body()["input"][0]["role"], "assistant");
    assert_eq!(
        lowered.body()["input"][0]["content"][0]["type"],
        "output_text"
    );
    assert_eq!(
        lowered.body()["input"][0]["content"][0]["text"],
        "prior answer"
    );

    let source_protocol = ProtocolId::new("openai.codex.responses")?;
    let source_family = ModelFamilyId::new("gpt")?;
    let source_model = ModelId::new("gpt-5.6-sol")?;
    let commentary_with_native = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![ConversationItem::new(
            id::<EntryId>(16)?,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new("working", Some(TextPhase::Commentary))?),
                native_output(
                    &source_protocol,
                    &source_family,
                    &source_model,
                    "private-commentary-marker",
                )?,
            ],
            Provenance::ModelOutput,
        )?])?,
        metadata()?,
    )
    .build()?;
    let lowered = lower_responses_request(
        &commentary_with_native,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    let wire = lowered.body().to_string();
    assert!(wire.contains("working"));
    assert!(!wire.contains("private-commentary-marker"));
    Ok(())
}

#[test]
fn codex_history_projects_portably_to_supergrok_without_native_bytes() -> TestResult {
    let source_protocol = ProtocolId::new("openai.codex.responses")?;
    let source_family = ModelFamilyId::new("gpt")?;
    let source_model = ModelId::new("gpt-5.6")?;
    let target_protocol = ProtocolId::new("xai.supergrok.responses")?;
    let target_family = ModelFamilyId::new("grok")?;
    let target_model = ModelId::new("grok-4.5")?;
    let history =
        cross_provider_history(&source_protocol, &source_family, &source_model, 600, true)?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(history)?,
        metadata()?,
    )
    .tools(vec![tool("lookup", false)?])
    .build()?;
    let canonical_before = serde_json::to_vec(request.input().history())?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &target_protocol,
            Some(&target_family),
            &target_model,
            ResponsesRequestPolicy::supergrok_stateless(),
            None,
        ),
    )?;
    let wire = lowered.body().to_string();
    assert!(wire.contains("portable answer"));
    assert!(wire.contains("portable result"));
    assert!(wire.contains("call-600"));
    assert!(!wire.contains("portable reasoning summary"));
    assert!(!wire.contains("private-"));
    assert_eq!(
        canonical_before,
        serde_json::to_vec(request.input().history())?
    );
    Ok(())
}

#[test]
fn grok_history_projects_portably_to_codex_without_foreign_reasoning() -> TestResult {
    let source_protocol = ProtocolId::new("xai.supergrok.responses")?;
    let source_family = ModelFamilyId::new("grok")?;
    let source_model = ModelId::new("grok-4.5")?;
    let target_protocol = ProtocolId::new("openai.codex.responses")?;
    let target_family = ModelFamilyId::new("gpt")?;
    let target_model = ModelId::new("gpt-5.6")?;
    let build = |include_reasoning| -> Result<ModelRequest, Box<dyn std::error::Error>> {
        Ok(crate::test_support::model_request_builder(
            ConversationView::from_items(cross_provider_history(
                &source_protocol,
                &source_family,
                &source_model,
                700,
                include_reasoning,
            )?)?,
            metadata()?,
        )
        .tools(vec![tool("lookup", false)?])
        .build()?)
    };
    let config = || {
        ResponsesRequestConfig::new(
            &target_protocol,
            Some(&target_family),
            &target_model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        )
    };
    let portable = build(true)?;
    let canonical_before = serde_json::to_vec(portable.input().history())?;
    let wire = lower_responses_request(&portable, config())?
        .body()
        .to_string();
    assert!(wire.contains("portable answer"));
    assert!(wire.contains("portable result"));
    assert!(wire.contains("call-700"));
    assert!(!wire.contains("portable reasoning summary"));
    assert!(!wire.contains("private-"));
    assert_eq!(
        canonical_before,
        serde_json::to_vec(portable.input().history())?
    );
    Ok(())
}

#[test]
fn target_compatible_native_output_without_artifact_uses_portable_semantics() -> TestResult {
    let protocol = ProtocolId::new("openai.codex.responses")?;
    let family = ModelFamilyId::new("gpt")?;
    let model = ModelId::new("gpt-5.6")?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(cross_provider_history(
            &protocol, &family, &model, 750, false,
        )?)?,
        metadata()?,
    )
    .tools(vec![tool("lookup", false)?])
    .build()?;
    let config = || {
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        )
    };
    let wire = lower_responses_request(&request, config())?
        .body()
        .to_string();
    assert!(wire.contains("portable answer"));
    assert!(wire.contains("portable result"));
    assert!(!wire.contains("private-"));

    let opaque_only = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![ConversationItem::new(
            id::<EntryId>(759)?,
            Actor::Assistant,
            vec![native_output(
                &protocol,
                &family,
                &model,
                "target-compatible-opaque-only",
            )?],
            Provenance::ModelOutput,
        )?])?,
        metadata()?,
    )
    .build()?;
    assert!(matches!(
        lower_responses_request(&opaque_only, config()),
        Err(ResponsesRequestError::Content(
            ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
        ))
    ));
    Ok(())
}

#[test]
fn incompatible_opaque_fallback_matrix_is_exact_in_both_directions() -> TestResult {
    let directions = [
        (
            ProtocolId::new("openai.codex.responses")?,
            ModelFamilyId::new("gpt")?,
            ModelId::new("gpt-5.6")?,
            ProtocolId::new("xai.supergrok.responses")?,
            ModelFamilyId::new("grok")?,
            ModelId::new("grok-4.5")?,
            ResponsesRequestPolicy::supergrok_stateless(),
        ),
        (
            ProtocolId::new("xai.supergrok.responses")?,
            ModelFamilyId::new("grok")?,
            ModelId::new("grok-4.5")?,
            ProtocolId::new("openai.codex.responses")?,
            ModelFamilyId::new("gpt")?,
            ModelId::new("gpt-5.6")?,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
        ),
    ];
    for (
        direction,
        (
            source_protocol,
            source_family,
            source_model,
            target_protocol,
            target_family,
            target_model,
            policy,
        ),
    ) in directions.into_iter().enumerate()
    {
        for (fallback_index, fallback) in [
            OpaqueFallback::Omit,
            OpaqueFallback::SemanticSummary("portable opaque summary".to_owned()),
            OpaqueFallback::CompatibleReplay,
        ]
        .into_iter()
        .enumerate()
        {
            let base = 900 + u128::try_from(direction * 10 + fallback_index)?;
            let opaque_only = crate::test_support::model_request_builder(
                ConversationView::from_items(vec![ConversationItem::new(
                    id::<EntryId>(base)?,
                    Actor::Assistant,
                    vec![native_output_with_fallback(
                        &source_protocol,
                        &source_family,
                        &source_model,
                        "private-fallback-marker",
                        fallback.clone(),
                    )?],
                    Provenance::ModelOutput,
                )?])?,
                metadata()?,
            )
            .build()?;
            let config = || {
                ResponsesRequestConfig::new(
                    &target_protocol,
                    Some(&target_family),
                    &target_model,
                    policy,
                    None,
                )
            };
            match &fallback {
                OpaqueFallback::CompatibleReplay => assert!(matches!(
                    lower_responses_request(&opaque_only, config()),
                    Err(ResponsesRequestError::Content(
                        ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
                    ))
                )),
                OpaqueFallback::Omit => assert_eq!(
                    lower_responses_request(&opaque_only, config())?.body()["input"],
                    json!([])
                ),
                OpaqueFallback::SemanticSummary(summary) => {
                    let lowered = lower_responses_request(&opaque_only, config())?;
                    assert_eq!(lowered.body()["input"][0]["role"], "assistant");
                    assert_eq!(
                        lowered.body()["input"][0]["content"][0],
                        json!({"type":"output_text","text":summary})
                    );
                }
                _ => unreachable!("closed test fallback"),
            }

            let mixed = crate::test_support::model_request_builder(
                ConversationView::from_items(vec![ConversationItem::new(
                    id::<EntryId>(base + 100)?,
                    Actor::Assistant,
                    vec![
                        ContentBlock::Text(TextBlock::new(
                            "portable sibling",
                            Some(TextPhase::Final),
                        )?),
                        native_output_with_fallback(
                            &source_protocol,
                            &source_family,
                            &source_model,
                            "private-mixed-marker",
                            fallback.clone(),
                        )?,
                    ],
                    Provenance::ModelOutput,
                )?])?,
                metadata()?,
            )
            .build()?;
            let mixed = lower_responses_request(&mixed, config())?;
            assert_eq!(mixed.body()["input"][0]["role"], "assistant");
            assert_eq!(
                mixed.body()["input"][0]["content"][0]["text"],
                "portable sibling"
            );
            let expected_parts =
                usize::from(matches!(fallback, OpaqueFallback::SemanticSummary(_))) + 1;
            assert_eq!(
                mixed.body()["input"][0]["content"]
                    .as_array()
                    .expect("message parts")
                    .len(),
                expected_parts
            );
            assert!(!mixed.body().to_string().contains("private-"));
        }

        let user_summary = crate::test_support::model_request_builder(
            ConversationView::from_items(vec![ConversationItem::new(
                id::<EntryId>(990 + u128::try_from(direction)?)?,
                Actor::User,
                vec![native_output_with_fallback(
                    &source_protocol,
                    &source_family,
                    &source_model,
                    "private-user-marker",
                    OpaqueFallback::SemanticSummary("portable user summary".to_owned()),
                )?],
                Provenance::UserInput,
            )?])?,
            metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &user_summary,
            ResponsesRequestConfig::new(
                &target_protocol,
                Some(&target_family),
                &target_model,
                policy,
                None,
            ),
        )?;
        assert_eq!(lowered.body()["input"][0]["role"], "user");
        assert_eq!(
            lowered.body()["input"][0]["content"][0],
            json!({"type":"input_text","text":"portable user summary"})
        );
    }
    Ok(())
}

#[test]
fn opaque_only_cross_target_semantics_fail_closed_without_payload_disclosure() -> TestResult {
    let cases = [
        (
            ProtocolId::new("openai.codex.responses")?,
            ModelFamilyId::new("gpt")?,
            ModelId::new("gpt-5.6")?,
            ProtocolId::new("xai.supergrok.responses")?,
            ModelFamilyId::new("grok")?,
            ModelId::new("grok-4.5")?,
            ResponsesRequestPolicy::supergrok_stateless(),
            800,
        ),
        (
            ProtocolId::new("xai.supergrok.responses")?,
            ModelFamilyId::new("grok")?,
            ModelId::new("grok-4.5")?,
            ProtocolId::new("openai.codex.responses")?,
            ModelFamilyId::new("gpt")?,
            ModelId::new("gpt-5.6")?,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            810,
        ),
    ];
    for (
        source_protocol,
        source_family,
        source_model,
        target_protocol,
        target_family,
        target_model,
        policy,
        entry,
    ) in cases
    {
        let request = crate::test_support::model_request_builder(
            ConversationView::from_items(vec![ConversationItem::new(
                id::<EntryId>(entry)?,
                Actor::Assistant,
                vec![native_output(
                    &source_protocol,
                    &source_family,
                    &source_model,
                    "secret-opaque-only-marker",
                )?],
                Provenance::ModelOutput,
            )?])?,
            metadata()?,
        )
        .build()?;
        let error = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &target_protocol,
                Some(&target_family),
                &target_model,
                policy,
                None,
            ),
        )
        .expect_err("opaque-only foreign semantics must fail closed");
        assert_eq!(
            error,
            ResponsesRequestError::Content(
                ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
            )
        );
        assert!(!error.to_string().contains("secret-opaque-only-marker"));
        assert!(!format!("{error:?}").contains("secret-opaque-only-marker"));
    }
    Ok(())
}

#[test]
fn codex_deltas_are_closed_policy_data() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let optional_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type":"object", "properties":{
            "source":{"type":"string"},
            "yield_time_ms":{"type":"integer","minimum":0,"maximum":10000}
        }, "required":["source"], "additionalProperties":false
    });
    let optional_tool = ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("exec_js")?,
            "1.0.0",
            "Run local JavaScript",
            ToolInputContract::structured(optional_schema.clone())?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?,
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    );
    let request =
        crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
            .tools(vec![optional_tool, tool("write_code", true)?])
            .limits(GenerationLimits::new(
                Some(NonZeroU32::new(512).expect("nonzero")),
                Vec::new(),
            )?)
            .tool_choice(ToolChoice::Specific(ToolName::new("write_code")?))
            .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    assert_eq!(lowered.body()["store"], false);
    assert!(lowered.body().get("max_output_tokens").is_none());
    assert_eq!(lowered.body()["tools"][0]["strict"], false);
    assert_eq!(
        lowered.body()["tools"][0]["parameters"]["required"],
        json!(["source"])
    );
    assert_eq!(
        lowered.body()["tools"][0]["parameters"]["properties"],
        optional_schema["properties"]
    );
    assert_eq!(lowered.body()["tools"][1]["type"], "custom");
    assert_eq!(
        lowered.body()["tool_choice"],
        json!({"type": "custom", "name": "write_code"})
    );
    assert!(
        lowered
            .custom_tools()
            .contains(&ToolName::new("write_code")?)
    );
    Ok(())
}

#[test]
fn grok_deltas_emit_exact_stateless_reasoning_wire() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(41, "Inspect the repository.")?])?,
        metadata()?,
    )
    .limits(GenerationLimits::new(
        Some(NonZeroU32::new(256).expect("nonzero")),
        Vec::new(),
    )?)
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Medium),
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let policy = ResponsesRequestPolicy::supergrok_stateless();
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
    )?;
    assert_eq!(
        lowered.body(),
        &json!({
            "model": "gpt-test",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Inspect the repository."}]
            }],
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "max_output_tokens": 256,
            "reasoning": {"effort": "medium", "summary": "concise"}
        })
    );
    assert!(lowered.body().get("tool_choice").is_none());
    assert!(lowered.body().get("parallel_tool_calls").is_none());
    Ok(())
}

#[test]
fn supergrok_lowers_instructions_as_first_input_system_message() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(10, "hello")?])?,
        metadata()?,
    )
    .instructions(vec![InstructionBlock::new(
        InstructionSource::Profile,
        "You are a precise coding agent.",
    )?])
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::supergrok_stateless(),
            None,
        ),
    )?;
    assert!(lowered.body().get("instructions").is_none());
    let input = lowered.body()["input"].as_array().ok_or("input")?;
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "system");
    assert!(
        input[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("precise coding agent"))
    );
    Ok(())
}

#[test]
fn supergrok_input_omits_output_only_status_on_reasoning_replay() -> TestResult {
    let (request, active) = active_replay_fixture(ResponsesStorageMode::Stateless, None)?;
    let (protocol, family, model) = target_values()?;
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &active,
        &request,
        operation_authority(
            &request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        ResponsesReplayKind::new("openai.output_item", 2),
        None,
        None,
        Timestamp::new(1_700_000_000, 0)?,
    )?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::supergrok_stateless(),
            Some(bridge.context()),
        ),
    )?;
    let reasoning = lowered.body()["input"]
        .as_array()
        .ok_or("input")?
        .iter()
        .find(|item| item["type"] == "reasoning")
        .ok_or("missing reasoning replay")?;
    assert_eq!(reasoning["encrypted_content"], "fixture-encrypted");
    assert_eq!(reasoning["id"], "fixture-reasoning");
    assert!(
        reasoning.get("status").is_none(),
        "Grok Build rejects reasoning status on input, got {reasoning}"
    );
    Ok(())
}

/// The official client sends `summary: "auto"` for every catalog model
/// that admits the parameter; the summaries it returns are the only wire
/// a Codex thought block can arrive on.
#[test]
fn codex_reasoning_requests_auto_summaries_and_always_includes_encrypted_replay() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request =
        crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
            .reasoning(ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::High),
                requirement: CapabilityRequirement::Required,
            })
            .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    assert_eq!(
        lowered.body()["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(
        lowered.body()["include"],
        json!(["reasoning.encrypted_content"])
    );
    Ok(())
}

#[test]
fn grok_emits_stateless_store_and_encrypted_include_like_the_pinned_reference() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request =
        crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
            .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::supergrok_stateless(),
            None,
        ),
    )?;
    assert!(lowered.body().get("reasoning").is_none());
    assert_eq!(
        lowered.body()["include"],
        json!(["reasoning.encrypted_content"])
    );
    assert_eq!(lowered.body()["store"], false);
    Ok(())
}

#[test]
fn grok_reasoning_policy_admits_xhigh_for_model_specific_upstream_gating() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let policy = ResponsesRequestPolicy::supergrok_stateless();
    let xhigh =
        crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
            .reasoning(ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::XHigh),
                requirement: CapabilityRequirement::Required,
            })
            .build()?;
    assert!(
        lower_responses_request(
            &xhigh,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None,),
        )
        .is_ok()
    );
    Ok(())
}

#[test]
fn xai_platform_policy_lowers_none_through_xhigh_and_refuses_the_rest() -> TestResult {
    type Lowered = Result<Result<Value, ResponsesRequestError>, Box<dyn std::error::Error>>;
    let (protocol, family, model) = target_values()?;
    let policy = ResponsesRequestPolicy::xai_platform_stateless("reasoning.encrypted_content");
    let lower = |effort| -> Lowered {
        let request =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .reasoning(ReasoningPolicy::Enabled {
                    effort: Some(effort),
                    requirement: CapabilityRequirement::Required,
                })
                .build()?;
        Ok(lower_responses_request(
            &request,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
        )
        .map(|lowered| lowered.body()["reasoning"].clone()))
    };
    for (effort, wire) in [
        (ReasoningEffort::None, "none"),
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::XHigh, "xhigh"),
    ] {
        assert_eq!(lower(effort)??, json!({ "effort": wire }), "{effort:?}");
    }
    for effort in [ReasoningEffort::Max, ReasoningEffort::Ultra] {
        assert!(matches!(
            lower(effort)?,
            Err(ResponsesRequestError::Option(
                ResponsesRequestOptionError::UnknownReasoningEffort
            ))
        ));
    }
    Ok(())
}

#[test]
fn cache_intent_fails_closed_when_policy_has_no_exact_wire_semantics() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request =
        crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
            .cache(CachePolicy::Enabled {
                requirement: CapabilityRequirement::Required,
            })
            .build()?;
    assert!(matches!(
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::supergrok_stateless(),
                None,
            ),
        ),
        Err(ResponsesRequestError::Option(
            ResponsesRequestOptionError::PromptCache
        ))
    ));

    let disabled =
        crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
            .cache(CachePolicy::Disabled)
            .build()?;
    for policy in [
        ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
        ResponsesRequestPolicy::supergrok_stateless(),
    ] {
        let lowered = lower_responses_request(
            &disabled,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
        )?;
        assert!(lowered.body().get("prompt_cache_key").is_none());
    }
    Ok(())
}

#[test]
fn affinity_key_and_gateway_session_emit_from_branch_identity() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let affinity = Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"test-affinity"])?;
    let request =
        crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
            .cache(CachePolicy::Disabled)
            .build()?;
    let codex = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        )
        .with_cache_affinity(Some(&affinity)),
    )?;
    assert_eq!(
        codex.body()["prompt_cache_key"],
        json!(affinity.to_string())
    );
    assert!(codex.body().get("session_id").is_none());
    assert!(codex.body().get("cache_control").is_none());

    let openrouter = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openrouter_stateless(),
            None,
        )
        .with_cache_affinity(Some(&affinity)),
    )?;
    assert_eq!(openrouter.body()["session_id"], json!(affinity.to_string()));
    assert!(openrouter.body().get("prompt_cache_key").is_none());
    assert!(openrouter.body().get("cache_control").is_none());
    Ok(())
}

#[test]
fn forked_branch_affinity_does_not_reuse_source_wire_keys() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let session = id::<SessionId>(90_001)?;
    let source_branch = id::<BranchId>(90_002)?;
    let fork_branch = id::<BranchId>(90_003)?;
    let source = ModelCacheAffinity::from_runtime_entropy(session, source_branch, 0, [0x41; 32])?;
    let fork = ModelCacheAffinity::from_runtime_entropy(session, fork_branch, 0, [0x42; 32])?;
    let recovered = ModelCacheAffinity::from_runtime_entropy(session, fork_branch, 0, [0x42; 32])?;
    assert_ne!(source.identity(), fork.identity());
    assert_eq!(fork.identity(), recovered.identity());
    let request =
        crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
            .cache(CachePolicy::Disabled)
            .build()?;
    let source_id = source.identity();
    let fork_id = fork.identity();

    let source_codex = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        )
        .with_cache_affinity(Some(&source_id)),
    )?;
    let fork_codex = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        )
        .with_cache_affinity(Some(&fork_id)),
    )?;
    assert_eq!(
        source_codex.body()["prompt_cache_key"],
        json!(source_id.to_string())
    );
    assert_eq!(
        fork_codex.body()["prompt_cache_key"],
        json!(fork_id.to_string())
    );
    assert_ne!(
        source_codex.body()["prompt_cache_key"],
        fork_codex.body()["prompt_cache_key"]
    );
    assert!(source_codex.body().get("session_id").is_none());
    assert!(fork_codex.body().get("session_id").is_none());

    let source_or = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openrouter_stateless(),
            None,
        )
        .with_cache_affinity(Some(&source_id)),
    )?;
    let fork_or = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openrouter_stateless(),
            None,
        )
        .with_cache_affinity(Some(&fork_id)),
    )?;
    assert_eq!(source_or.body()["session_id"], json!(source_id.to_string()));
    assert_eq!(fork_or.body()["session_id"], json!(fork_id.to_string()));
    assert_ne!(source_or.body()["session_id"], fork_or.body()["session_id"]);
    assert!(source_or.body().get("prompt_cache_key").is_none());
    assert!(fork_or.body().get("prompt_cache_key").is_none());
    assert!(source_or.body().get("cache_control").is_none());
    assert!(fork_or.body().get("cache_control").is_none());
    Ok(())
}

#[test]
fn inline_image_bytes_lower_to_base64_data_urls_for_users_and_tool_results() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let image =
        plakous_model::ImageBlock::new("image/png", MediaSource::bytes(b"\x89PNG".to_vec())?)?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![ConversationItem::new(
            id::<EntryId>(21)?,
            Actor::User,
            vec![
                ContentBlock::Text(TextBlock::new("what is this", None)?),
                ContentBlock::Image(image.clone()),
            ],
            Provenance::UserInput,
        )?])?,
        metadata()?,
    )
    .build()?;
    let lower = |policy| {
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
        )
    };
    let lowered = lower(ResponsesRequestPolicy::openai_codex(
        true,
        false,
        "reasoning.encrypted_content",
    ))?;
    assert_eq!(
        lowered.body()["input"][0]["content"],
        json!([
            {"type": "input_text", "text": "what is this"},
            {"type": "input_image", "image_url": "data:image/png;base64,iVBORw=="}
        ])
    );
    assert!(lower(ResponsesRequestPolicy::openrouter_stateless()).is_err());
    assert_eq!(
        lower_tool_result(&[ToolResultPart::Image(image)])?,
        json!([{"type": "input_image", "image_url": "data:image/png;base64,iVBORw=="}])
    );
    Ok(())
}

#[test]
fn multimodal_tool_result_lowers_exact_data_urls_and_detail() -> TestResult {
    let image_url = "data:image/png;base64,AAAA";
    let audio_url = "data:audio/wav;base64,BBBB";
    let lowered = lower_tool_result(&[
        ToolResultPart::Image(plakous_model::ImageBlock::with_detail(
            "image/png",
            MediaSource::data_url(image_url)?,
            Some(ImageDetailHint::Original),
        )?),
        ToolResultPart::Audio(plakous_model::AudioBlock::new(
            "audio/wav",
            MediaSource::data_url(audio_url)?,
        )?),
    ])?;
    assert_eq!(
        lowered,
        json!([
            {"type":"input_image", "image_url":image_url, "detail":"original"},
            {"type":"input_audio", "audio_url":audio_url}
        ])
    );
    Ok(())
}

#[test]
fn ordered_function_and_custom_tool_groups_lower_exactly() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let long_result = "x".repeat(4_096);
    let function_call = ToolCallBlock::new(
        id::<CallId>(20)?,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-function".to_owned()),
            None,
            ToolCallerLineage::Direct,
        )?,
        r#"{"q":"rust"}"#,
    )?;
    let custom_call = ToolCallBlock::new(
        id::<CallId>(21)?,
        ToolName::new("write_code")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-custom".to_owned()),
            None,
            ToolCallerLineage::Direct,
        )?,
        serde_json::to_string("println!(\"héllo\")")?,
    )?;
    let call_item = ConversationItem::new(
        id::<EntryId>(22)?,
        Actor::Assistant,
        vec![
            ContentBlock::ToolCall(function_call.clone()),
            ContentBlock::ToolCall(custom_call.clone()),
        ],
        Provenance::ModelOutput,
    )?;
    let result_item = ConversationItem::new(
        id::<EntryId>(23)?,
        Actor::Tool,
        vec![
            ContentBlock::ToolResult(ToolResultBlock::new(
                function_call.call_id(),
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new(
                    long_result.clone(),
                    None,
                )?)],
                function_call.correlation().clone(),
            )?),
            ContentBlock::ToolResultUpdate(ToolResultUpdateBlock::new(
                custom_call.call_id(),
                ToolExecutionOwner::Local,
                TextBlock::new("halfway", None)?,
                custom_call.correlation().clone(),
            )?),
            ContentBlock::ToolResult(ToolResultBlock::new(
                custom_call.call_id(),
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new("written", None)?)],
                custom_call.correlation().clone(),
            )?),
        ],
        Provenance::ToolExecution,
    )?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![call_item, result_item])?,
        workflow_metadata()?,
    )
    .tools(vec![tool("lookup", false)?, tool("write_code", true)?])
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    let input = lowered.body()["input"].as_array().expect("input");
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[1]["type"], "custom_tool_call");
    assert_eq!(input[1]["input"], "println!(\"héllo\")");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["output"], long_result);
    assert_eq!(input[3]["type"], "custom_tool_call_output");
    assert_eq!(input[3]["name"], "write_code");
    assert_eq!(input[3]["output"], "halfway");
    assert_eq!(input[4]["type"], "custom_tool_call_output");

    let invalid_custom = ToolCallBlock::new(
        id::<CallId>(27)?,
        ToolName::new("write_code")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("invalid-custom".to_owned()),
            None,
            ToolCallerLineage::Direct,
        )?,
        r#"{"code":"print(1)"}"#,
    )?;
    let invalid_request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![
            ConversationItem::new(
                id::<EntryId>(28)?,
                Actor::Assistant,
                vec![ContentBlock::ToolCall(invalid_custom.clone())],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                id::<EntryId>(29)?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    invalid_custom.call_id(),
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new("written", None)?)],
                    invalid_custom.correlation().clone(),
                )?)],
                Provenance::ToolExecution,
            )?,
        ])?,
        metadata()?,
    )
    .tools(vec![tool("write_code", true)?])
    .build()?;
    assert!(matches!(
        lower_responses_request(
            &invalid_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
                None,
            ),
        ),
        Err(ResponsesRequestError::Content(
            ResponsesRequestContentError::InvalidCustomToolArguments
        ))
    ));
    Ok(())
}

/// Rebind re-projection: a Codex Code Mode `exec` cell (a
/// UTF-8 custom call, canonical as a JSON string literal) replayed onto a
/// Responses target that exposes no custom tool — Codex Direct, Grok —
/// lowers as a `function_call` carrying the original program text, and its
/// yields fold into the one `function_call_output` in source order.
#[test]
fn code_mode_exec_cell_replays_as_a_function_call_on_a_direct_target() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let program = "const out = await tools.exec_command({cmd: 'pwd'});\nyield_(out);";
    let exec_call = ToolCallBlock::new(
        id::<CallId>(40)?,
        ToolName::new("exec")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("call_exec_1".to_owned()),
            None,
            ToolCallerLineage::Direct,
        )?,
        serde_json::to_string(program)?,
    )?;
    assert!(matches!(exec_call.arguments(), Some(Value::String(_))));
    let wait_call = ToolCallBlock::new(
        id::<CallId>(41)?,
        ToolName::new("wait")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("call_wait_1".to_owned()),
            None,
            ToolCallerLineage::Direct,
        )?,
        r#"{"cell":"c1"}"#,
    )?;
    let call_item = ConversationItem::new(
        id::<EntryId>(42)?,
        Actor::Assistant,
        vec![
            ContentBlock::ToolCall(exec_call.clone()),
            ContentBlock::ToolCall(wait_call.clone()),
        ],
        Provenance::ModelOutput,
    )?;
    let result_item = ConversationItem::new(
        id::<EntryId>(43)?,
        Actor::Tool,
        vec![
            ContentBlock::ToolResultUpdate(ToolResultUpdateBlock::new(
                exec_call.call_id(),
                ToolExecutionOwner::Local,
                TextBlock::new("/home/op", None)?,
                exec_call.correlation().clone(),
            )?),
            ContentBlock::ToolResult(ToolResultBlock::new(
                exec_call.call_id(),
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new("cell done", None)?)],
                exec_call.correlation().clone(),
            )?),
            ContentBlock::ToolResult(ToolResultBlock::new(
                wait_call.call_id(),
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new("waited", None)?)],
                wait_call.correlation().clone(),
            )?),
        ],
        Provenance::ToolExecution,
    )?;
    // The Direct target exposes only structured functions.
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![call_item, result_item])?,
        workflow_metadata()?,
    )
    .tools(vec![tool("exec_command", false)?])
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Medium),
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    let input = lowered.body()["input"].as_array().expect("input");
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["name"], "exec");
    assert_eq!(
        input[0]["arguments"], program,
        "original string input, verbatim"
    );
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["name"], "wait");
    assert_eq!(input[1]["arguments"], r#"{"cell":"c1"}"#);
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "call_exec_1");
    assert_eq!(
        input[2]["output"],
        json!([
            {"type":"input_text","text":"/home/op"},
            {"type":"input_text","text":"cell done"}
        ]),
        "the cell's yield folds into its one output in source order"
    );
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_wait_1");
    assert_eq!(input[3]["output"], "waited");
    assert!(
        input
            .iter()
            .all(|item| item["type"] != "custom_tool_call_output"),
        "a function call never owns a custom_tool_call_output: {input:?}"
    );
    let affinity = Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"codex-lite-round"])?;
    let lite = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex_lite("reasoning.encrypted_content"),
            None,
        )
        .with_cache_affinity(Some(&affinity)),
    )?;
    let lite_input = lite.body()["input"].as_array().ok_or("lite input")?;
    assert_eq!(lite_input[0]["type"], "additional_tools");
    assert_eq!(lite_input[0]["role"], "developer");
    assert_eq!(lite_input[0]["tools"][0]["type"], "namespace");
    assert_eq!(lite_input[0]["tools"][0]["name"], "functions");
    assert_eq!(
        lite_input[0]["tools"][0]["tools"][0]["name"],
        "exec_command"
    );
    assert_eq!(lite_input[1]["type"], "function_call");
    assert_eq!(lite_input[1]["call_id"], "call_exec_1");
    assert_eq!(lite_input[3]["type"], "function_call_output");
    assert_eq!(lite_input[3]["call_id"], "call_exec_1");
    assert_eq!(lite_input[4]["call_id"], "call_wait_1");
    assert!(lite.body().get("tools").is_none());
    assert!(lite.body().get("instructions").is_none());
    assert_eq!(lite.body()["parallel_tool_calls"], false);
    assert_eq!(lite.body()["reasoning"]["context"], "all_turns");
    assert_eq!(lite.body()["text"]["verbosity"], "low");
    Ok(())
}

#[test]
fn fresh_codex_body_matches_portable_reasoning_projection() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let affinity = Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"offline-pass"])?;
    let reasoning = ContentBlock::Reasoning(ReasoningBlock::new(
        ReasoningVisibility::OpaqueReplay,
        None,
        Some(ReplayEnvelope::new(
            protocol.clone(),
            Some(family.clone()),
            ReplayScope::ExactAccountModel {
                model: model.clone(),
                account: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
            },
            ReplayKindId::new("responses.output_item")?,
            1,
            DataClassification::Sensitive,
            RawValue::from_string(
                json!({
                    "type": "reasoning", "id": "fixture-reasoning",
                    "encrypted_content": "fixture-encrypted", "summary": []
                })
                .to_string(),
            )?,
        )?),
    )?);
    let call = ToolCallBlock::new(
        id::<CallId>(930)?,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        r#"{"q":"cache"}"#,
    )?;
    let items = vec![
        user(931, "keep reading")?,
        ConversationItem::new(
            id::<EntryId>(932)?,
            Actor::Assistant,
            vec![reasoning.clone()],
            Provenance::ModelOutput,
        )?,
        ConversationItem::new(
            id::<EntryId>(933)?,
            Actor::Assistant,
            vec![
                reasoning,
                ContentBlock::Text(TextBlock::new("looking up", None)?),
                ContentBlock::ToolCall(call.clone()),
            ],
            Provenance::ModelOutput,
        )?,
        ConversationItem::new(
            id::<EntryId>(934)?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock::new(
                call.call_id(),
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new("result", None)?)],
                ToolCorrelation::default(),
            )?)],
            Provenance::ToolExecution,
        )?,
    ];
    // The agent's portable_compaction_history drops reasoning and elides
    // items with no surviving blocks; keep that exact projection here
    // (pinned by portable_history_drops_reasoning_and_elides_reasoning_only_items).
    let mut portable = Vec::new();
    for item in &items {
        let blocks = item
            .blocks()
            .iter()
            .filter(|block| !matches!(block, ContentBlock::Reasoning(_)))
            .cloned()
            .collect::<Vec<_>>();
        if !blocks.is_empty() {
            portable.push(ConversationItem::new(
                item.id(),
                item.actor(),
                blocks,
                item.provenance(),
            )?);
        }
    }
    let lower = |items| -> Result<LoweredResponsesRequest, Box<dyn std::error::Error>> {
        let request = crate::test_support::model_request_builder(
            ConversationView::from_items(items)?,
            metadata()?,
        )
        .build()?;
        Ok(lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
            .with_cache_affinity(Some(&affinity)),
        )?)
    };
    let unchanged = lower(items)?;
    let portable = lower(portable)?;
    assert_eq!(
        unchanged.body_bytes()?,
        portable.body_bytes()?,
        "Fix A must compare the real serialized Codex body"
    );
    assert!(!String::from_utf8(unchanged.body_bytes()?)?.contains("fixture-encrypted"));
    Ok(())
}

#[test]
fn result_before_call_fails_with_exact_protected_group_error() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let call = ToolCallBlock::new(
        id::<CallId>(30)?,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        r#"{"q":"order"}"#,
    )?;
    let result = ToolResultBlock::new(
        call.call_id(),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
        ToolCorrelation::default(),
    )?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![
            ConversationItem::new(
                id::<EntryId>(31)?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(result)],
                Provenance::ToolExecution,
            )?,
            ConversationItem::new(
                id::<EntryId>(32)?,
                Actor::Assistant,
                vec![ContentBlock::ToolCall(call)],
                Provenance::ModelOutput,
            )?,
        ])?,
        metadata()?,
    )
    .tools(vec![tool("lookup", false)?])
    .build()?;
    assert!(matches!(
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
                None,
            ),
        ),
        Err(ResponsesRequestError::ProtectedTool(
            ResponsesProtectedToolError::ResultBeforeCall
        ))
    ));
    Ok(())
}

// `output_replay_group_is_binding_checked_and_exact` was deleted here: it
// was `#[cfg(any())]` — compiled out unconditionally and silently green —
// and it no longer builds, because `ResponsesReplayContext::new` has since
// grown the request authority, the output coordinates, the provider
// continuation and an expiry. Reviving it would mean writing a new test
// against the current contract, not restoring this one; the binding checks
// it once covered are exercised by the replay tests below.

#[test]
fn provider_stored_without_active_artifact_replays_full_canonical_request() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(45, "first")?, user(46, "second")?])?,
        metadata()?,
    )
    .build()?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_platform(
                ResponsesStoragePolicy::ProviderStored,
                false,
                false,
            ),
            None,
        ),
    )?;
    assert_eq!(lowered.replay_mode(), &ResponsesReplayMode::Full);
    assert_eq!(lowered.body()["input"].as_array().expect("input").len(), 2);
    assert!(lowered.body().get("previous_response_id").is_none());
    Ok(())
}

#[test]
fn openai_platform_programmatic_request_and_replay_are_exact_in_both_storage_modes() -> TestResult {
    let (protocol, family, _) = target_values()?;
    let model = ModelId::new("gpt-5.6-sol")?;
    let base_tool = tool_with_concurrency("lookup", false, ToolConcurrency::Serial)?;
    let programmatic_tool = ConfiguredTool::new(
        base_tool.definition().clone(),
        base_tool.execution(),
        ToolCallerDisposition::ProviderProgrammaticOnly,
    );
    let producing_request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![user(145, "look up one value")?])?,
        metadata()?,
    )
    .tools(vec![programmatic_tool.clone()])
    .hosted_tools(vec![HostedTool::ProgrammaticComposition])
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let initial = lower_responses_request(
        &producing_request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_platform(
                ResponsesStoragePolicy::Stateless,
                false,
                false,
            ),
            None,
        ),
    )?;
    assert_eq!(
        initial.body()["tools"],
        json!([
            {
                "type":"function", "name":"lookup", "description":"lookup description",
                // No `$schema`: the wire projection strips the dialect URI.
                "parameters":{
                    "type":"object", "properties":{"q":{"type":"string"}},
                    "required":["q"], "additionalProperties":false
                },
                "strict":false, "allowed_callers":["programmatic"]
            },
            {"type":"programmatic_tool_calling"}
        ])
    );

    let program_native = json!({
        "type":"program", "id":"prog_1", "call_id":"pcall_1",
        "code":"await lookup({q:'x'})", "fingerprint":"opaque-fingerprint-1"
    });
    let call_native = json!({
        "type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"lookup",
        "arguments":"{\"q\":\"x\"}",
        "caller":{"type":"program","caller_id":"pcall_1"}
    });
    let replay = |native: &Value| {
        Ok::<_, Box<dyn std::error::Error>>(ReplayEnvelope::new(
            protocol.clone(),
            Some(family.clone()),
            ReplayScope::ExactAccountModel {
                model: model.clone(),
                account: Digest::sha256(
                    DigestDomain::ModelBinding,
                    1,
                    &[b"responses-ptc-test-route"],
                )?,
            },
            ReplayKindId::new("openai.output_item")?,
            2,
            DataClassification::Sensitive,
            RawValue::from_string(native.to_string())?,
        )?)
    };
    let program_replay = replay(&program_native)?;
    let call_replay = replay(&call_native)?;
    let lineage = ToolCallerLineage::provider_programmatic(
        NamespaceId::new("openai.responses.program")?,
        "pcall_1",
    )?;
    let call = ToolCallBlock::new(
        id::<CallId>(146)?,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(Some("call_1".to_owned()), None, lineage.clone())?,
        r#"{"q":"x"}"#,
    )?;
    let program_item = ConversationItem::new(
        id::<EntryId>(147)?,
        Actor::Assistant,
        vec![ContentBlock::Opaque(OpaqueBlock::new(
            program_replay.clone(),
            OpaqueFallback::CompatibleReplay,
        )?)],
        Provenance::ModelOutput,
    )?;
    let call_item = ConversationItem::new(
        id::<EntryId>(148)?,
        Actor::Assistant,
        vec![
            ContentBlock::ToolCall(call.clone()),
            ContentBlock::Opaque(OpaqueBlock::new(
                call_replay.clone(),
                OpaqueFallback::CompatibleReplay,
            )?),
        ],
        Provenance::ModelOutput,
    )?;
    let result_item = ConversationItem::new(
        id::<EntryId>(149)?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock::new(
            call.call_id(),
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
            ToolCorrelation::new(Some("call_1".to_owned()), None, lineage)?,
        )?)],
        Provenance::ToolExecution,
    )?;
    let next_request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![user(145, "look up one value")?],
                )?,
                HistorySegment::new(
                    HistorySegmentKind::ReasoningReplay,
                    vec![program_item.clone()],
                )?,
                HistorySegment::new(
                    HistorySegmentKind::ToolExchange,
                    vec![call_item.clone(), result_item.clone()],
                )?,
            ])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .tools(vec![programmatic_tool.clone()])
    .hosted_tools(vec![HostedTool::ProgrammaticComposition])
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;

    let second_call_native = json!({
        "type":"function_call", "id":"fc_2", "call_id":"call_2", "name":"lookup",
        "arguments":"{\"q\":\"y\"}",
        "caller":{"type":"program","caller_id":"pcall_1"}
    });
    let second_call_replay = replay(&second_call_native)?;
    let second_lineage = ToolCallerLineage::provider_programmatic(
        NamespaceId::new("openai.responses.program")?,
        "pcall_1",
    )?;
    let second_call = ToolCallBlock::new(
        id::<CallId>(150)?,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(Some("call_2".to_owned()), None, second_lineage.clone())?,
        r#"{"q":"y"}"#,
    )?;
    let second_call_item = ConversationItem::new(
        id::<EntryId>(151)?,
        Actor::Assistant,
        vec![
            ContentBlock::ToolCall(second_call.clone()),
            ContentBlock::Opaque(OpaqueBlock::new(
                second_call_replay.clone(),
                OpaqueFallback::CompatibleReplay,
            )?),
        ],
        Provenance::ModelOutput,
    )?;
    let second_result_item = ConversationItem::new(
        id::<EntryId>(152)?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock::new(
            second_call.call_id(),
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new("found again", None)?)],
            ToolCorrelation::new(Some("call_2".to_owned()), None, second_lineage)?,
        )?)],
        Provenance::ToolExecution,
    )?;
    let mut final_segments = next_request.input().history().segments().to_vec();
    final_segments.push(HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![second_call_item, second_result_item],
    )?);
    let final_request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(final_segments)?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .tools(vec![programmatic_tool])
    .hosted_tools(vec![HostedTool::ProgrammaticComposition])
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let now = Timestamp::new(1_700_000_000, 0)?;
    for storage in [
        ResponsesStorageMode::ProviderStored,
        ResponsesStorageMode::Stateless,
    ] {
        let policy = if storage == ResponsesStorageMode::ProviderStored {
            ResponsesStoragePolicy::ProviderStored
        } else {
            ResponsesStoragePolicy::Stateless
        };
        let artifact = output_artifact(
            &producing_request,
            &next_request,
            "responses-ptc-test-route",
            vec![program_replay.clone(), call_replay.clone()],
            storage,
            "response-ptc-first-pause",
            None,
            now,
            b"openai-platform-ptc-replay",
        )?;
        let active = active_artifact(&artifact)?;
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &next_request,
            operation_authority(&next_request, "responses-ptc-test-route", policy)?,
            ResponsesReplayKind::new("openai.output_item", 2),
            None,
            (storage == ResponsesStorageMode::ProviderStored)
                .then(|| ResponsesReplayKind::new("openai.previous_response_id", 1)),
            now,
        )?;
        assert_eq!(
            bridge.hosted_programs().live_program("pcall_1"),
            Some(("prog_1", "opaque-fingerprint-1"))
        );
        let continued = lower_responses_request(
            &next_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(policy, false, false),
                Some(bridge.context()),
            ),
        )?;
        if storage == ResponsesStorageMode::ProviderStored {
            assert!(matches!(
                continued.replay_mode(),
                ResponsesReplayMode::PreviousResponse {
                    prefix_items: 3,
                    ..
                }
            ));
            assert_eq!(
                continued.body()["previous_response_id"],
                "response-ptc-first-pause"
            );
            assert_eq!(
                continued.body()["input"],
                json!([{"type":"function_call_output","call_id":"call_1","output":"found","caller":{"type":"program","caller_id":"pcall_1"}}])
            );
        } else {
            assert_eq!(continued.replay_mode(), &ResponsesReplayMode::Full);
            assert_eq!(
                continued.body()["input"],
                json!([
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"look up one value"}]},
                    program_native,
                    call_native,
                    {"type":"function_call_output","call_id":"call_1","output":"found","caller":{"type":"program","caller_id":"pcall_1"}}
                ])
            );
            assert_eq!(
                continued.body()["input"][1]["fingerprint"],
                "opaque-fingerprint-1"
            );
        }

        // The operation-budget wrap-up over this same programmatic history
        // withdraws tool selection only, so it keeps the bound replay and
        // continues the live program. Fresh lowering has no form for
        // hosted program items or provider-programmatic calls.
        let wrap_up =
            crate::test_support::model_request_builder(next_request.input().clone(), metadata()?)
                .tools(next_request.tools().to_vec())
                .hosted_tools(next_request.hosted_tools().to_vec())
                .resolved_harness_plan_identity(harness_plan_identity()?)
                .tool_choice(ToolChoice::None)
                .build()?;
        let wrap_up_bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &wrap_up,
            operation_authority(&wrap_up, "responses-ptc-test-route", policy)?,
            ResponsesReplayKind::new("openai.output_item", 2),
            None,
            (storage == ResponsesStorageMode::ProviderStored)
                .then(|| ResponsesReplayKind::new("openai.previous_response_id", 1)),
            now,
        )?;
        assert_eq!(
            wrap_up_bridge.hosted_programs().live_program("pcall_1"),
            Some(("prog_1", "opaque-fingerprint-1"))
        );
        let wrapped = lower_responses_request(
            &wrap_up,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(policy, false, false),
                Some(wrap_up_bridge.context()),
            ),
        )?;
        assert_eq!(wrapped.body()["tool_choice"], "none");
        assert_eq!(wrapped.body()["tools"], continued.body()["tools"]);
        assert_eq!(wrapped.body()["input"], continued.body()["input"]);
        assert_eq!(wrapped.replay_mode(), continued.replay_mode());
        let fresh = lower_responses_request(
            &wrap_up,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(policy, false, false),
                None,
            ),
        );
        assert!(matches!(
            fresh.map_err(|error| {
                crate::ResponsesAttemptBuildError::Request(error).preparation_error_kind()
            }),
            Err(plakous_model::ModelPreparationErrorKind::ProtectedHistory)
        ));

        let second_artifact = output_artifact(
            &next_request,
            &final_request,
            "responses-ptc-test-route",
            vec![second_call_replay.clone()],
            storage,
            "response-ptc-second-pause",
            None,
            now,
            b"openai-platform-ptc-second-replay",
        )?;
        assert_ne!(
            artifact.production().covered_projection(),
            second_artifact.production().covered_projection(),
            "the second pause must advance artifact coverage"
        );
        assert_eq!(artifact.production().covered_projection_items(), 3);
        assert_eq!(second_artifact.production().covered_projection_items(), 5);
        let second_active = active_artifact(&second_artifact)?;
        let second_bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &second_active,
            &final_request,
            operation_authority(&final_request, "responses-ptc-test-route", policy)?,
            ResponsesReplayKind::new("openai.output_item", 2),
            None,
            (storage == ResponsesStorageMode::ProviderStored)
                .then(|| ResponsesReplayKind::new("openai.previous_response_id", 1)),
            now,
        )?;
        assert_eq!(second_bridge.hosted_programs().live_program_count(), 1);
        assert_eq!(
            second_bridge.hosted_programs().live_program("pcall_1"),
            Some(("prog_1", "opaque-fingerprint-1"))
        );
        let final_program_output = json!({
            "type":"program_output", "id":"program_output_1", "call_id":"pcall_1",
            "result":"found + found again", "status":"completed"
        });
        let mut completed_programs = second_bridge.hosted_programs().clone();
        completed_programs.admit_replayed_output(
            &parse_output_item::<ResponsesHostDecodeError>(&final_program_output)?,
            crate::ResponsesCanonicalPolicy::openai_platform(),
        )?;
        assert_eq!(completed_programs.live_program_count(), 0);
        let final_continuation = lower_responses_request(
            &final_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(policy, false, false),
                Some(second_bridge.context()),
            ),
        )?;
        if storage == ResponsesStorageMode::ProviderStored {
            assert!(matches!(
                final_continuation.replay_mode(),
                ResponsesReplayMode::PreviousResponse {
                    prefix_items: 5,
                    ..
                }
            ));
            assert_eq!(
                final_continuation.body()["previous_response_id"],
                "response-ptc-second-pause"
            );
            assert_eq!(
                final_continuation.body()["input"],
                json!([{"type":"function_call_output","call_id":"call_2","output":"found again","caller":{"type":"program","caller_id":"pcall_1"}}])
            );
        } else {
            assert_eq!(final_continuation.replay_mode(), &ResponsesReplayMode::Full);
            assert!(
                final_continuation
                    .body()
                    .get("previous_response_id")
                    .is_none()
            );
            assert_eq!(
                final_continuation.body()["input"],
                json!([
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"look up one value"}]},
                    program_native,
                    call_native,
                    {"type":"function_call_output","call_id":"call_1","output":"found","caller":{"type":"program","caller_id":"pcall_1"}},
                    second_call_native,
                    {"type":"function_call_output","call_id":"call_2","output":"found again","caller":{"type":"program","caller_id":"pcall_1"}}
                ])
            );
        }

        let wrong_program = replay(&json!({
            "type":"program", "id":"prog_1", "call_id":"pcall_1",
            "code":"await lookup({q:'x'})", "fingerprint":"wrong-fingerprint"
        }))?;
        let wrong_caller = replay(&json!({
            "type":"function_call", "id":"fc_1", "call_id":"call_1",
            "name":"lookup", "arguments":"{\"q\":\"x\"}",
            "caller":{"type":"program","caller_id":"wrong-program"}
        }))?;
        let mutated_views = [
            ConversationView::from_items(vec![
                user(145, "look up one value")?,
                ConversationItem::new(
                    id::<EntryId>(147)?,
                    Actor::Assistant,
                    vec![ContentBlock::Opaque(OpaqueBlock::new(
                        wrong_program.clone(),
                        OpaqueFallback::CompatibleReplay,
                    )?)],
                    Provenance::ModelOutput,
                )?,
                call_item.clone(),
                result_item.clone(),
            ])?,
            ConversationView::from_items(vec![
                user(145, "look up one value")?,
                program_item.clone(),
                ConversationItem::new(
                    id::<EntryId>(148)?,
                    Actor::Assistant,
                    vec![
                        ContentBlock::ToolCall(call.clone()),
                        ContentBlock::Opaque(OpaqueBlock::new(
                            wrong_caller.clone(),
                            OpaqueFallback::CompatibleReplay,
                        )?),
                    ],
                    Provenance::ModelOutput,
                )?,
                result_item.clone(),
            ])?,
            ConversationView::from_items(vec![
                user(145, "look up one value")?,
                call_item.clone(),
                program_item.clone(),
                result_item.clone(),
            ])?,
        ];
        let mutated_replays = [
            vec![wrong_program, call_replay.clone()],
            vec![program_replay.clone(), wrong_caller],
            vec![call_replay.clone(), program_replay.clone()],
        ];
        for (mutation_index, (mutated_view, mutated_replay)) in
            mutated_views.into_iter().zip(mutated_replays).enumerate()
        {
            let mutated_request =
                crate::test_support::model_request_builder(mutated_view, metadata()?)
                    .tools(vec![ConfiguredTool::new(
                        base_tool.definition().clone(),
                        base_tool.execution(),
                        ToolCallerDisposition::ProviderProgrammaticOnly,
                    )])
                    .hosted_tools(vec![HostedTool::ProgrammaticComposition])
                    .resolved_harness_plan_identity(harness_plan_identity()?)
                    .build()?;
            let mutated = output_artifact(
                &producing_request,
                &mutated_request,
                "responses-ptc-test-route",
                mutated_replay,
                storage,
                "response-ptc-first-pause",
                None,
                now,
                format!("openai-platform-ptc-mutation-{mutation_index}").as_bytes(),
            )?;
            assert!(
                ResponsesArtifactReplayBridge::from_active_artifact(
                    &active_artifact(&mutated)?,
                    if mutation_index == 0 {
                        &next_request
                    } else {
                        &mutated_request
                    },
                    operation_authority(
                        if mutation_index == 0 {
                            &next_request
                        } else {
                            &mutated_request
                        },
                        "responses-ptc-test-route",
                        policy,
                    )?,
                    ResponsesReplayKind::new("openai.output_item", 2),
                    None,
                    (storage == ResponsesStorageMode::ProviderStored)
                        .then(|| ResponsesReplayKind::new("openai.previous_response_id", 1)),
                    now,
                )
                .is_err(),
                "storage {storage:?} replay mutation {mutation_index} was admitted"
            );
        }
    }
    Ok(())
}

#[test]
fn active_artifact_wrapper_binding_revision_family_and_bytes_fail_closed() -> TestResult {
    let now = Timestamp::new(1_700_000_000, 0)?;
    let expires = Timestamp::new(1_800_000_000, 0)?;
    let (request, active) =
        active_replay_fixture(ResponsesStorageMode::ProviderStored, Some(expires))?;
    let binding = active.binding();
    let current_binding = operation_authority(
        &request,
        "responses-test-route",
        ResponsesStoragePolicy::ProviderStored,
    )?
    .artifact_binding()
    .identity();
    assert_ne!(binding, current_binding);
    let bytes = active.bytes().to_vec();
    let mut tampered_bytes = bytes.clone();
    let last = tampered_bytes
        .last_mut()
        .ok_or_else(|| std::io::Error::other("empty active bytes"))?;
    *last ^= 0x01;
    let wrappers = [
        (
            "family",
            ActiveFamilyArtifact::new(
                "messages",
                RESPONSES_ARTIFACT_SCHEMA_REVISION,
                RESPONSES_ARTIFACT_CODEC_REVISION,
                binding,
                bytes.clone(),
            )?,
        ),
        (
            "schema revision",
            ActiveFamilyArtifact::new(
                "responses",
                RESPONSES_ARTIFACT_SCHEMA_REVISION.saturating_add(1),
                RESPONSES_ARTIFACT_CODEC_REVISION,
                binding,
                bytes.clone(),
            )?,
        ),
        (
            "codec revision",
            ActiveFamilyArtifact::new(
                "responses",
                RESPONSES_ARTIFACT_SCHEMA_REVISION,
                RESPONSES_ARTIFACT_CODEC_REVISION.saturating_add(1),
                binding,
                bytes.clone(),
            )?,
        ),
        (
            "generic binding",
            ActiveFamilyArtifact::new(
                "responses",
                RESPONSES_ARTIFACT_SCHEMA_REVISION,
                RESPONSES_ARTIFACT_CODEC_REVISION,
                Digest::sha256(DigestDomain::ModelBinding, 1, &[b"foreign-neutral-binding"])?,
                bytes,
            )?,
        ),
        (
            "rebound current-operation wrapper",
            ActiveFamilyArtifact::new(
                "responses",
                RESPONSES_ARTIFACT_SCHEMA_REVISION,
                RESPONSES_ARTIFACT_CODEC_REVISION,
                current_binding,
                active.bytes().to_vec(),
            )?,
        ),
        (
            "family bytes",
            ActiveFamilyArtifact::new(
                "responses",
                RESPONSES_ARTIFACT_SCHEMA_REVISION,
                RESPONSES_ARTIFACT_CODEC_REVISION,
                binding,
                tampered_bytes,
            )?,
        ),
    ];
    for (axis, wrapper) in &wrappers {
        assert!(
            ResponsesArtifactReplayBridge::from_active_artifact(
                wrapper,
                &request,
                operation_authority(
                    &request,
                    "responses-test-route",
                    ResponsesStoragePolicy::ProviderStored,
                )?,
                ResponsesReplayKind::new("openai.output_item", 2),
                None,
                Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                now,
            )
            .is_err(),
            "invalid active artifact {axis} was admitted"
        );
    }
    Ok(())
}

#[test]
fn active_artifact_expiry_storage_and_replay_kinds_fail_closed() -> TestResult {
    let now = Timestamp::new(1_700_000_000, 0)?;
    let expires = Timestamp::new(1_800_000_000, 0)?;
    let (request, active) =
        active_replay_fixture(ResponsesStorageMode::ProviderStored, Some(expires))?;
    let admit = |storage, output, opaque, previous, at| {
        ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &request,
            operation_authority(&request, "responses-test-route", storage).expect("authority"),
            output,
            opaque,
            previous,
            at,
        )
    };
    assert!(matches!(
        admit(
            ResponsesStoragePolicy::ProviderStored,
            ResponsesReplayKind::new("openai.output_item", 2),
            None,
            Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
            expires,
        ),
        Err(ResponsesRequestError::Replay(
            ResponsesReplayError::ArtifactExpired
        ))
    ));
    for (axis, rejected) in [
        (
            "storage",
            admit(
                ResponsesStoragePolicy::Stateless,
                ResponsesReplayKind::new("openai.output_item", 2),
                None,
                Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                now,
            ),
        ),
        (
            "output kind revision",
            admit(
                ResponsesStoragePolicy::ProviderStored,
                ResponsesReplayKind::new("openai.output_item", 3),
                None,
                Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                now,
            ),
        ),
        (
            "opaque kind",
            admit(
                ResponsesStoragePolicy::ProviderStored,
                ResponsesReplayKind::new("openai.output_item", 2),
                Some(ResponsesReplayKind::new("openai.opaque_output_item", 1)),
                Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                now,
            ),
        ),
        (
            "missing previous-response kind",
            admit(
                ResponsesStoragePolicy::ProviderStored,
                ResponsesReplayKind::new("openai.output_item", 2),
                None,
                None,
                now,
            ),
        ),
    ] {
        assert!(
            rejected.is_err(),
            "active artifact {axis} drift was admitted"
        );
    }
    Ok(())
}

#[test]
fn active_artifact_turns_one_two_three_and_reopen_use_latest_stored_response() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let replay = ReplayEnvelope::new(
        protocol.clone(),
        Some(family.clone()),
        ReplayScope::ExactAccountModel {
            model: model.clone(),
            account: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
        },
        ReplayKindId::new("openai.output_item")?,
        2,
        DataClassification::Sensitive,
        RawValue::from_string(
            json!({
                "id": "reasoning-1",
                "type": "reasoning",
                "status": "completed",
                "summary": [],
                "encrypted_content": "encrypted-1"
            })
            .to_string(),
        )?,
    )?;
    let producing_prefix =
        HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(49, "start")?])?;
    let producing_request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![producing_prefix.clone()])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let output_segment = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            id::<EntryId>(50)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                replay.clone(),
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let next_request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![
                producing_prefix,
                output_segment,
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![user(51, "continue")?],
                )?,
            ])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let now = Timestamp::new(1_700_000_000, 0)?;
    let expires = Timestamp::new(1_800_000_000, 0)?;
    let stored = output_artifact(
        &producing_request,
        &next_request,
        "responses-test-route",
        vec![replay.clone()],
        ResponsesStorageMode::ProviderStored,
        "response-1",
        Some(expires),
        now,
        b"stored-round-trip",
    )?;
    let output_kind = ResponsesReplayKind::new("openai.output_item", 2);
    let previous_kind = ResponsesReplayKind::new("openai.previous_response_id", 1);
    let stored = active_artifact(&stored)?;
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &stored,
        &next_request,
        operation_authority(
            &next_request,
            "responses-test-route",
            ResponsesStoragePolicy::ProviderStored,
        )?,
        output_kind,
        None,
        Some(previous_kind),
        now,
    )?;
    let lowered = lower_responses_request(
        &next_request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_platform(
                ResponsesStoragePolicy::ProviderStored,
                false,
                false,
            ),
            Some(bridge.context()),
        ),
    )?;
    assert_eq!(lowered.body()["previous_response_id"], "response-1");
    assert_eq!(lowered.body()["input"].as_array().expect("input").len(), 1);
    assert_eq!(lowered.body()["input"][0]["content"][0]["text"], "continue");

    let replay_two = ReplayEnvelope::new(
        protocol.clone(),
        Some(family.clone()),
        ReplayScope::ExactAccountModel {
            model: model.clone(),
            account: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
        },
        ReplayKindId::new("openai.output_item")?,
        2,
        DataClassification::Sensitive,
        RawValue::from_string(
            json!({
                "id": "reasoning-2",
                "type": "reasoning",
                "status": "completed",
                "summary": [],
                "encrypted_content": "encrypted-2"
            })
            .to_string(),
        )?,
    )?;
    let mut third_segments = next_request.input().history().segments().to_vec();
    third_segments.push(HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            id::<EntryId>(52)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                replay_two.clone(),
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?],
    )?);
    third_segments.push(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![user(53, "third turn")?],
    )?);
    let third_request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(third_segments)?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let latest = output_artifact(
        &next_request,
        &third_request,
        "responses-test-route",
        vec![replay_two],
        ResponsesStorageMode::ProviderStored,
        "response-2",
        Some(expires),
        now,
        b"stored-third-turn",
    )?;
    let reopened: ActiveFamilyArtifact =
        serde_json::from_value(serde_json::to_value(active_artifact(&latest)?)?)?;
    let reopened_bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &reopened,
        &third_request,
        operation_authority(
            &third_request,
            "responses-test-route",
            ResponsesStoragePolicy::ProviderStored,
        )?,
        output_kind,
        None,
        Some(previous_kind),
        now,
    )?;
    let third = lower_responses_request(
        &third_request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_platform(
                ResponsesStoragePolicy::ProviderStored,
                false,
                false,
            ),
            Some(reopened_bridge.context()),
        ),
    )?;
    assert_eq!(third.body()["previous_response_id"], "response-2");
    assert_eq!(third.body()["input"].as_array().expect("input").len(), 1);
    assert_eq!(third.body()["input"][0]["content"][0]["text"], "third turn");

    assert!(matches!(
        ResponsesArtifactReplayBridge::from_active_artifact(
            &stored,
            &next_request,
            operation_authority(
                &next_request,
                "responses-test-route",
                ResponsesStoragePolicy::ProviderStored,
            )?,
            output_kind,
            None,
            Some(previous_kind),
            expires,
        ),
        Err(ResponsesRequestError::Replay(
            ResponsesReplayError::ArtifactExpired
        ))
    ));

    let stateless = output_artifact(
        &producing_request,
        &next_request,
        "responses-test-route",
        vec![replay],
        ResponsesStorageMode::Stateless,
        "unused-stateless-response",
        Some(expires),
        now,
        b"stateless-round-trip",
    )?;
    let stateless = active_artifact(&stateless)?;
    let stateless_bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &stateless,
        &next_request,
        operation_authority(
            &next_request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        output_kind,
        None,
        None,
        now,
    )?;
    let replayed = lower_responses_request(
        &next_request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            Some(stateless_bridge.context()),
        ),
    )?;
    assert_eq!(replayed.body()["input"][1]["id"], "reasoning-1");
    assert_eq!(replayed.body()["input"][1]["type"], "reasoning");

    let foreign = operation_authority(
        &next_request,
        "foreign-route",
        ResponsesStoragePolicy::Stateless,
    )?;
    assert!(matches!(
        ResponsesArtifactReplayBridge::from_active_artifact(
            &stateless,
            &next_request,
            foreign,
            output_kind,
            None,
            None,
            now,
        ),
        Err(ResponsesRequestError::Replay(
            ResponsesReplayError::ArtifactTargetMismatch { axis: "route" }
        ))
    ));

    // The operation-budget wrap-up withdraws tool selection over the same
    // history. The binding leaves tool selection out of its prefix
    // identity, so the wrap-up still continues this artifact.
    let wrap_up_request =
        crate::test_support::model_request_builder(next_request.input().clone(), metadata()?)
            .resolved_harness_plan_identity(harness_plan_identity()?)
            .tool_choice(ToolChoice::None)
            .build()?;
    let wrap_up = ResponsesArtifactReplayBridge::from_active_artifact(
        &stateless,
        &wrap_up_request,
        operation_authority(
            &wrap_up_request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        output_kind,
        None,
        None,
        now,
    )?;
    let replayed_wrap_up = lower_responses_request(
        &wrap_up_request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            Some(wrap_up.context()),
        ),
    )?;
    assert_eq!(replayed_wrap_up.body()["input"][1]["id"], "reasoning-1");
    assert!(replayed_wrap_up.body().to_string().contains("encrypted-1"));
    // Any other stable-prefix difference is not this artifact's
    // continuation: the route prepares fresh and still lowers the complete
    // history.
    let drifted_request =
        crate::test_support::model_request_builder(next_request.input().clone(), metadata()?)
            .resolved_harness_plan_identity(harness_plan_identity()?)
            .instructions(vec![InstructionBlock::new(
                InstructionSource::System,
                "answer briefly",
            )?])
            .tool_choice(ToolChoice::None)
            .build()?;
    let drifted = ResponsesArtifactReplayBridge::from_active_artifact(
        &stateless,
        &drifted_request,
        operation_authority(
            &drifted_request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        output_kind,
        None,
        None,
        now,
    );
    assert!(matches!(
        drifted,
        Err(ResponsesRequestError::Replay(
            ResponsesReplayError::ArtifactStablePrefixMismatch
        ))
    ));
    assert!(drifted.is_err_and(|error| error.artifact_is_not_for_this_request()));
    // Fresh lowering of that history as the canonicalizer stores it
    // (native reasoning is `Omit`) sends every portable item and no
    // native reasoning.
    let segments = wrap_up_request.input().history().segments();
    let ContentBlock::Opaque(reasoning) = &segments[1].items()[0].blocks()[0] else {
        return Err("fixture reasoning is not opaque".into());
    };
    let stored_reasoning = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            id::<EntryId>(50)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                reasoning.replay().clone(),
                OpaqueFallback::Omit,
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let stored_wrap_up = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![
                segments[0].clone(),
                stored_reasoning,
                segments[2].clone(),
            ])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .tool_choice(ToolChoice::None)
    .build()?;
    let fresh = lower_responses_request(
        &stored_wrap_up,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            None,
        ),
    )?;
    let fresh = fresh.body().to_string();
    assert!(fresh.contains("start") && fresh.contains("continue"));
    assert!(!fresh.contains("encrypted-1"));
    // A hard target axis still wins over the soft prefix miss.
    assert!(matches!(
        ResponsesArtifactReplayBridge::from_active_artifact(
            &stateless,
            &wrap_up_request,
            operation_authority(
                &wrap_up_request,
                "foreign-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            output_kind,
            None,
            None,
            now,
        ),
        Err(ResponsesRequestError::Replay(
            ResponsesReplayError::ArtifactTargetMismatch { axis: "route" }
        ))
    ));
    // Every other hard axis also wins over a tool-free prefix: a foreign
    // tool plan and a different storage mode.
    let foreign_plan = Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"responses-foreign-tool-plan"],
    )?;
    let foreign_plan_wrap_up =
        crate::test_support::model_request_builder(next_request.input().clone(), metadata()?)
            .resolved_harness_plan_identity(foreign_plan)
            .tool_choice(ToolChoice::None)
            .build()?;
    let foreign_tool_plan = ResponsesArtifactReplayBridge::from_active_artifact(
        &stateless,
        &foreign_plan_wrap_up,
        operation_authority(
            &foreign_plan_wrap_up,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        output_kind,
        None,
        None,
        now,
    );
    assert!(
        matches!(
            foreign_tool_plan,
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::ArtifactTargetMismatch { axis: "tool-plan" }
            ))
        ),
        "tool-plan must stay hard under a tool-free prefix: {foreign_tool_plan:?}"
    );
    let foreign_storage = ResponsesArtifactReplayBridge::from_active_artifact(
        &stateless,
        &wrap_up_request,
        operation_authority(
            &wrap_up_request,
            "responses-test-route",
            ResponsesStoragePolicy::ProviderStored,
        )?,
        output_kind,
        None,
        None,
        now,
    );
    assert!(
        matches!(
            foreign_storage,
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::ArtifactTargetMismatch {
                    axis: "storage-mode"
                }
            ))
        ),
        "storage-mode must stay hard under a tool-free prefix: {foreign_storage:?}"
    );

    let original = next_request.input().history().segments();
    let malformed_histories = [
        vec![original[0].clone()],
        vec![
            original[0].clone(),
            original[2].clone(),
            original[1].clone(),
        ],
        vec![
            original[1].clone(),
            original[0].clone(),
            original[2].clone(),
        ],
    ];
    for (index, segments) in malformed_histories.into_iter().enumerate() {
        let malformed = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(segments)?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let authority = operation_authority(
            &malformed,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?;
        assert!(
            ResponsesArtifactReplayBridge::from_active_artifact(
                &stateless,
                &malformed,
                authority,
                output_kind,
                None,
                None,
                now,
            )
            .is_err(),
            "gap/reorder/truncation mutation {index} was accepted"
        );
    }
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the vertical test proves the complete project-codec-append-next-request chain in both modes"
)]
fn project_codec_append_and_next_request_are_exact_in_both_storage_modes() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let replay_projection = ResponsesReplayProjection::new(
        protocol.clone(),
        Some(family.clone()),
        model.clone(),
        ReplayKindId::new("openai.output_item")?,
        2,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
    )?;
    let response = ResponsesResponse::parse(
        &json!({
            "id":"response-vertical","object":"response","status":"completed","model":"gpt-test",
            "output":[{
                "id":"reasoning-vertical","type":"reasoning","status":"completed","summary":[],
                "encrypted_content":"encrypted-vertical"
            }]
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )?;
    let prefix = HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(90, "first")?])?;
    let producing_request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![prefix.clone()])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let output_kind = ResponsesReplayKind::new("openai.output_item", 2);
    let previous_kind = ResponsesReplayKind::new("openai.previous_response_id", 1);
    let now = Timestamp::new(1_700_000_000, 0)?;
    let provider_request = ResponsesProviderRequestBinding::new(
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"request-host-project-codec-physical"],
        )?,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"request-host-project-codec-terminal"],
        )?,
        None,
    )?;

    for (index, storage) in [
        ResponsesStorageMode::ProviderStored,
        ResponsesStorageMode::Stateless,
    ]
    .into_iter()
    .enumerate()
    {
        let projected = project_terminal_response(
            response.clone(),
            &replay_projection,
            storage,
            provider_request.clone(),
        )?;
        let replay = projected
            .artifact_candidate()
            .and_then(|candidate| candidate.replay().first())
            .map(|item| item.envelope().clone())
            .ok_or_else(|| std::io::Error::other("missing projected replay"))?;
        let canonical = ConversationItem::new(
            id::<EntryId>(91 + u128::try_from(index)?)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                replay,
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?;
        let paired = projected.pair_canonical(&producing_request, vec![canonical])?;
        let (_, _, _, paired_artifact) = paired.into_parts();
        let paired_artifact =
            paired_artifact.ok_or_else(|| std::io::Error::other("missing paired artifact"))?;
        let canonical_output = paired_artifact.canonical_output().to_vec();
        let output_segment =
            HistorySegment::new(HistorySegmentKind::ReasoningReplay, canonical_output)?;
        let next = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![
                    prefix.clone(),
                    output_segment,
                    HistorySegment::new(
                        HistorySegmentKind::RecentIntent,
                        vec![user(93 + u128::try_from(index)?, "next")?],
                    )?,
                ])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let plan = descriptor("responses-test-route")?
            .validate_request(producing_request.clone(), ValidationMode::Strict)?;
        let covered_projection = next.input().canonical_item_prefix_identity(2)?;
        let sealed = paired_artifact.seal(&plan, covered_projection)?;
        let (
            _canonical_output,
            production,
            replay_contract,
            replay,
            continuation,
            candidate_storage,
        ) = sealed.into_parts();
        assert_eq!(production.output_items(), 1);
        let artifact = ResponsesArtifact::new(
            operation_binding(&producing_request, "responses-test-route")?,
            production,
            replay_contract,
            replay,
            continuation,
            None,
            candidate_storage,
            None,
            now,
        )?;
        let active = active_artifact(&artifact)?;
        let (policy, previous) = if storage == ResponsesStorageMode::ProviderStored {
            (ResponsesStoragePolicy::ProviderStored, Some(previous_kind))
        } else {
            (ResponsesStoragePolicy::Stateless, None)
        };
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &next,
            operation_authority(&next, "responses-test-route", policy)?,
            output_kind,
            None,
            previous,
            now,
        )?;
        let request_policy = if storage == ResponsesStorageMode::ProviderStored {
            ResponsesRequestPolicy::openai_platform(policy, false, false)
        } else {
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content")
        };
        let lowered = lower_responses_request(
            &next,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                request_policy,
                Some(bridge.context()),
            ),
        )?;
        let input = lowered.body()["input"].as_array().expect("input");
        if storage == ResponsesStorageMode::ProviderStored {
            assert_eq!(input.len(), 1);
            assert_eq!(lowered.body()["previous_response_id"], "response-vertical");
        } else {
            assert_eq!(input.len(), 3);
            assert_eq!(input[1]["id"], "reasoning-vertical");
        }
    }
    Ok(())
}

#[test]
fn latest_artifact_authorizes_full_prefix_and_disambiguates_same_digest_groups() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let now = Timestamp::new(1_700_000_000, 0)?;
    let output_kind = ResponsesReplayKind::new("openai.output_item", 2);
    let raw = |id: &str| -> Result<ReplayEnvelope, Box<dyn std::error::Error>> {
        Ok(ReplayEnvelope::new(
            protocol.clone(),
            Some(family.clone()),
            ReplayScope::ExactAccountModel {
                model: model.clone(),
                account: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
            },
            ReplayKindId::new("openai.output_item")?,
            2,
            DataClassification::Sensitive,
            RawValue::from_string(
                json!({
                    "id": id,
                    "type": "reasoning",
                    "status":"completed",
                    "summary": [],
                    "encrypted_content": format!("encrypted-{id}")
                })
                .to_string(),
            )?,
        )?)
    };
    let replay_one = raw("reasoning-1")?;
    let replay_two = replay_one.clone();
    let prefix = HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(80, "first")?])?;
    let output_one = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            id::<EntryId>(81)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                replay_one.clone(),
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let between = HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(82, "second")?])?;
    let output_two = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            id::<EntryId>(83)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                replay_two.clone(),
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let after = HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(84, "third")?])?;
    let request_at_two = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![prefix.clone(), output_one.clone(), between.clone()])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let next_request = crate::test_support::model_request_builder(
        ConversationView::from_shared(
            CanonicalHistory::new(vec![prefix, output_one, between, output_two, after])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let artifact_two = output_artifact(
        &request_at_two,
        &next_request,
        "responses-test-route",
        vec![replay_two],
        ResponsesStorageMode::Stateless,
        "unused-second-group",
        None,
        now,
        b"same-digest-group-two",
    )?;
    let encoded_two = ResponsesArtifactCodec::encode(&artifact_two)?;
    let active_two = active_artifact(&artifact_two)?;
    assert!(!String::from_utf8_lossy(&encoded_two).contains("reasoning-1"));
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &active_two,
        &next_request,
        operation_authority(
            &next_request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?,
        output_kind,
        None,
        None,
        now,
    )?;
    let lowered = lower_responses_request(
        &next_request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            Some(bridge.context()),
        ),
    )?;
    let input = lowered.body()["input"].as_array().expect("input");
    assert_eq!(input.len(), 5);
    assert_eq!(input[1]["id"], "reasoning-1");
    assert_eq!(input[3]["id"], "reasoning-1");

    let grok = lower_responses_request(
        &next_request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::supergrok_stateless(),
            Some(bridge.context()),
        ),
    )?;
    let grok_input = grok.body()["input"].as_array().expect("Grok input");
    assert_eq!(grok_input.len(), 5);
    assert_eq!(grok_input[1]["id"], "reasoning-1");
    assert_eq!(grok_input[3]["id"], "reasoning-1");

    Ok(())
}

#[test]
fn strict_router_policy_is_closed_and_rejects_unbounded_or_ambient_values() -> TestResult {
    let policy = ResponsesRouterPolicy::strict("anthropic/claude", "deny", true)?;
    assert_eq!(
        policy.wire_value(),
        serde_json::json!({
            "only": ["anthropic/claude"],
            "order": ["anthropic/claude"],
            "allow_fallbacks": false,
            "require_parameters": true,
            "data_collection": "deny",
            "zdr": true,
        })
    );
    for invalid in ["", "auto", "anthropic claude", &"x".repeat(257)] {
        assert!(ResponsesRouterPolicy::strict(invalid, "deny", true).is_err());
    }
    assert!(ResponsesRouterPolicy::strict("anthropic/claude", "Allow", true).is_err());
    assert_eq!(
        policy.execution_identity()?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"responses-router-policy-v1",
                b"anthropic/claude",
                b"deny",
                b"zdr",
            ],
        )?
    );
    assert_ne!(
        ResponsesRequestPolicy::openrouter_stateless().execution_identity()?,
        ResponsesRequestPolicy::supergrok_stateless().execution_identity()?
    );
    Ok(())
}

#[test]
fn gateway_managed_router_policy_is_exact_endpoint_neutral_and_identity_bearing() -> TestResult {
    let policy = ResponsesRouterPolicy::gateway_managed(None, None)?;
    let wire = policy.wire_value();
    assert_eq!(
        wire,
        serde_json::json!({
            "allow_fallbacks": false,
            "require_parameters": true,
        })
    );
    assert!(wire.get("only").is_none());
    assert!(wire.get("order").is_none());
    assert!(wire.get("endpoint").is_none());
    assert_ne!(
        policy.execution_identity()?,
        ResponsesRouterPolicy::strict("anthropic/claude", "deny", true)?.execution_identity()?
    );
    let selected = ResponsesRouterPolicy::gateway_managed(Some("deny".to_owned()), Some(true))?;
    assert_eq!(
        selected.wire_value(),
        serde_json::json!({
            "allow_fallbacks": false,
            "require_parameters": true,
            "data_collection": "deny",
            "zdr": true,
        })
    );
    let selected_false =
        ResponsesRouterPolicy::gateway_managed(Some("deny".to_owned()), Some(false))?;
    let selected_zdr_only = ResponsesRouterPolicy::gateway_managed(None, Some(true))?;
    assert_ne!(policy.execution_identity()?, selected.execution_identity()?);
    assert_ne!(
        selected.execution_identity()?,
        selected_false.execution_identity()?
    );
    assert_ne!(
        policy.execution_identity()?,
        selected_zdr_only.execution_identity()?
    );
    assert!(ResponsesRouterPolicy::gateway_managed(Some("DENY".to_owned()), None).is_err());
    Ok(())
}

#[test]
fn a_route_derived_output_bound_compiles_where_the_policy_emits_one() -> TestResult {
    let mut selected = selected_responses_configuration()?;
    let mut behavior = selected.behavior().clone();
    behavior.output_bound = OutputBoundDisposition::DerivedFromRoute(NonZeroU32::MIN);
    selected = SelectedProviderConfiguration::new(selected.route().clone(), behavior)?;
    let descriptor = descriptor("responses-test-route")?;
    compile_provider_configuration(
        &selected,
        descriptor.clone(),
        configuration_authority(&descriptor)?,
        ResponsesRequestPolicy::supergrok_stateless(),
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum LongReplayHistory {
    CanonicalItems,
    ReplayEnvelopes,
    ReplayBytes,
}

#[allow(
    clippy::too_many_lines,
    reason = "one fixture exercises all historical bounds through sealing and lowering"
)]
fn long_history_replay(
    route: &str,
    policy: ResponsesRequestPolicy,
    bound: LongReplayHistory,
) -> TestResult {
    let (protocol, family, model) = target_values()?;
    let now = Timestamp::new(1_700_000_000, 0)?;
    let (plain_count, replay_count, encrypted_bytes) = match bound {
        LongReplayHistory::CanonicalItems => (4_097, 0, 1),
        LongReplayHistory::ReplayEnvelopes => (1, 4_097, 1),
        LongReplayHistory::ReplayBytes => (4, 4, 800_000),
    };
    let replay = |bytes: usize| -> Result<ReplayEnvelope, Box<dyn std::error::Error>> {
        Ok(ReplayEnvelope::new(
            protocol.clone(),
            Some(family.clone()),
            ReplayScope::ExactAccountModel {
                model: model.clone(),
                account: Digest::sha256(DigestDomain::ModelBinding, 1, &[route.as_bytes()])?,
            },
            ReplayKindId::new("openai.output_item")?,
            2,
            DataClassification::Sensitive,
            RawValue::from_string(
                json!({
                    "id":"long-reasoning", "type":"reasoning", "status":"completed",
                    "summary":[], "encrypted_content":"e".repeat(bytes)
                })
                .to_string(),
            )?,
        )?)
    };
    let reasoning_item = |index: u128,
                          envelope: ReplayEnvelope|
     -> Result<ConversationItem, Box<dyn std::error::Error>> {
        Ok(ConversationItem::new(
            id::<EntryId>(index)?,
            Actor::Assistant,
            vec![ContentBlock::Reasoning(ReasoningBlock::new(
                ReasoningVisibility::OpaqueReplay,
                None,
                Some(envelope),
            )?)],
            Provenance::ModelOutput,
        )?)
    };
    let plain = (0..plain_count)
        .map(|i| user(10_000 + i, &"h".repeat(encrypted_bytes)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut segments = plain
        .chunks(2_048)
        .map(|items| HistorySegment::new(HistorySegmentKind::RecentIntent, items.to_vec()))
        .collect::<Result<Vec<_>, _>>()?;
    if replay_count > 0 {
        let historical = replay(encrypted_bytes)?;
        let items = (0..replay_count)
            .map(|i| reasoning_item(20_000 + i, historical.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let response_items = if matches!(bound, LongReplayHistory::ReplayBytes) {
            1
        } else {
            2_048
        };
        for chunk in items.chunks(response_items) {
            segments.push(HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                chunk.to_vec(),
            )?);
        }
    }
    let build = |segments| -> Result<ModelRequest, Box<dyn std::error::Error>> {
        Ok(crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(segments)?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?)
    };
    let producing = build(segments.clone())?;
    let latest = replay(1)?;
    segments.push(HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![reasoning_item(30_000, latest.clone())?],
    )?);
    segments.push(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![user(30_001, "next")?],
    )?);
    let next = build(segments)?;
    let storage = replay::storage_mode(policy.storage());
    let artifact = output_artifact(
        &producing,
        &next,
        route,
        vec![latest],
        storage,
        "long-response",
        None,
        now,
        b"long-history",
    )?;
    let active = active_artifact(&artifact)?;
    // The active artifact has one small output; only the historical prefix is large.
    assert!(active.bytes().len() < 16_384);
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &active,
        &next,
        operation_authority(&next, route, policy.storage())?,
        ResponsesReplayKind::new("openai.output_item", 2),
        None,
        (storage == ResponsesStorageMode::ProviderStored)
            .then_some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
        now,
    )?;
    let lowered = lower_responses_request(
        &next,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            policy,
            Some(bridge.context()),
        ),
    )?;
    let input = lowered.body()["input"].as_array().expect("input");
    if storage == ResponsesStorageMode::ProviderStored {
        assert_eq!(input.len(), 1);
        assert_eq!(lowered.body()["previous_response_id"], "long-response");
    } else {
        assert_eq!(
            input.len(),
            usize::try_from(plain_count + replay_count + 2)?
        );
        assert_eq!(input[input.len() - 2]["encrypted_content"], "e");
        if replay_count > 0 {
            assert_eq!(
                input[usize::try_from(plain_count)?]["encrypted_content"],
                "e".repeat(encrypted_bytes)
            );
        }
    }
    // Fresh and compaction lowering have no aggregate artifact authority. They
    // omit unbound reasoning, not a bounded tail of the portable conversation.
    let fresh = lower_responses_request(
        &next,
        ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
    )?;
    assert_eq!(
        fresh.body()["input"].as_array().expect("fresh input").len(),
        usize::try_from(plain_count + 1)?
    );
    assert!(
        !fresh.body()["input"]
            .to_string()
            .contains("encrypted_content")
    );
    let compaction = crate::test_support::model_request_builder(next.input().clone(), metadata()?)
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .tool_choice(ToolChoice::None)
        .build()?;
    let compact = lower_responses_request(
        &compaction,
        ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
    )?;
    assert_eq!(compact.body()["input"], fresh.body()["input"]);
    Ok(())
}

macro_rules! long_history_route_tests {
    ($name:ident, $policy:expr) => {
        mod $name {
            use super::*;
            #[test]
            fn canonical_items() -> TestResult {
                long_history_replay(
                    stringify!($name),
                    $policy,
                    LongReplayHistory::CanonicalItems,
                )
            }
            #[test]
            fn replay_envelopes() -> TestResult {
                long_history_replay(
                    stringify!($name),
                    $policy,
                    LongReplayHistory::ReplayEnvelopes,
                )
            }
            #[test]
            fn replay_bytes() -> TestResult {
                long_history_replay(stringify!($name), $policy, LongReplayHistory::ReplayBytes)
            }
        }
    };
}

long_history_route_tests!(
    long_codex,
    ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content")
);
long_history_route_tests!(
    long_openai_platform,
    ResponsesRequestPolicy::openai_platform_stateless()
);
long_history_route_tests!(
    long_openai_stored,
    ResponsesRequestPolicy::openai_platform(ResponsesStoragePolicy::ProviderStored, false, false)
);
long_history_route_tests!(
    long_supergrok,
    ResponsesRequestPolicy::supergrok_stateless()
);
long_history_route_tests!(
    long_xai_platform,
    ResponsesRequestPolicy::xai_platform_stateless("reasoning.encrypted_content")
);
long_history_route_tests!(
    long_openrouter,
    ResponsesRequestPolicy::openrouter_stateless()
);

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the regression follows a fresh account-B round through artifact publication and the next request"
)]
fn new_account_artifact_does_not_resurrect_previous_account_reasoning() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let route = "responses-test-route";
    let now = Timestamp::new(1_700_000_000, 0)?;
    let (old_account, new_account) = replay_account_fixtures()?;
    let raw = |marker: &str,
               account: Digest|
     -> Result<ReplayEnvelope, Box<dyn std::error::Error>> {
        Ok(ReplayEnvelope::new(protocol.clone(), Some(family.clone()), ReplayScope::ExactAccountModel { model: model.clone(), account },
            ReplayKindId::new("openai.output_item")?, 2, DataClassification::Sensitive,
            RawValue::from_string(json!({"id":marker,"type":"reasoning","status":"completed","summary":[],"encrypted_content":marker}).to_string())?)?)
    };
    let reasoning = |entry, replay| -> Result<HistorySegment, Box<dyn std::error::Error>> {
        Ok(HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                id::<EntryId>(entry)?,
                Actor::Assistant,
                vec![ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::OpaqueReplay,
                    None,
                    Some(replay),
                )?)],
                Provenance::ModelOutput,
            )?],
        )?)
    };
    let build = |segments| -> Result<ModelRequest, Box<dyn std::error::Error>> {
        Ok(crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(segments)?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?)
    };
    // Rebind preserves canonical history and clears the active artifact. A fork
    // of that branch or retained same-model import can carry the same prefix.
    let mut segments = vec![
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![user(40_000, "start")?],
        )?,
        reasoning(40_001, raw("account-A-ciphertext", old_account)?)?,
    ];
    let fresh_request = build(segments.clone())?;
    let policy = ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content");
    let fresh = lower_responses_request(
        &fresh_request,
        ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
    )?;
    assert!(
        !fresh.body()["input"]
            .to_string()
            .contains("account-A-ciphertext")
    );
    let new_replay = raw("account-B-ciphertext", new_account)?;
    segments.push(reasoning(40_002, new_replay.clone())?);
    segments.push(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![user(40_003, "next")?],
    )?);
    let next = build(segments)?;
    let authority =
        |request: &ModelRequest| -> Result<ResponsesRequestAuthority, Box<dyn std::error::Error>> {
            let descriptor = descriptor(route)?;
            let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
            Ok(ResponsesRequestAuthority::from_admitted_operation(
                &descriptor,
                &plan,
                request,
                new_account,
                ResponsesSemanticRevisions::new(1, 1, 1)?,
                None,
                ResponsesStoragePolicy::Stateless,
            )?)
        };
    let plan =
        descriptor(route)?.validate_request(fresh_request.clone(), ValidationMode::Strict)?;
    let production = ResponsesArtifactProduction::from_admitted_response(
        &fresh_request,
        &plan,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"account-rebind-request"],
        )?,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"account-rebind-response"],
        )?,
        1,
        next.input().canonical_item_prefix_identity(3)?.identity(),
    )?;
    let contract = ResponsesReplayContract::new(ResponsesArtifactReplayKind::new(
        new_replay.kind().clone(),
        new_replay.version(),
    )?);
    let artifact = ResponsesArtifact::new(
        authority(&fresh_request)?.artifact_binding().clone(),
        production,
        contract,
        vec![ResponsesReplayItem::new(
            ResponsesReplayPurpose::OutputItem,
            0,
            new_replay,
        )],
        None,
        None,
        ResponsesStorageMode::Stateless,
        None,
        now,
    )?;
    let active = active_artifact(&artifact)?;
    let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
        &active,
        &next,
        authority(&next)?,
        ResponsesReplayKind::new("openai.output_item", 2),
        None,
        None,
        now,
    )?;
    let lowered = lower_responses_request(
        &next,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            policy,
            Some(bridge.context()),
        ),
    )?;
    let input = lowered.body()["input"].to_string();
    assert!(
        !input.contains("account-A-ciphertext"),
        "new account artifact resurrected old account reasoning"
    );
    assert!(input.contains("account-B-ciphertext"));
    Ok(())
}

fn replay_account_fixtures() -> Result<(Digest, Digest), Box<dyn std::error::Error>> {
    Ok((
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"new-account"])?,
    ))
}

fn replay_with_scope(
    source: &ReplayEnvelope,
    scope: ReplayScope,
) -> Result<ReplayEnvelope, Box<dyn std::error::Error>> {
    Ok(ReplayEnvelope::new(
        source.protocol().clone(),
        source.model_family().cloned(),
        scope,
        source.kind().clone(),
        source.version(),
        source.classification(),
        RawValue::from_string(source.payload().get().to_owned())?,
    )?)
}

#[test]
fn artifact_append_rejects_foreign_account_and_accountless_output() -> TestResult {
    let (request, active) = active_replay_fixture(ResponsesStorageMode::Stateless, None)?;
    let artifact = ResponsesArtifactCodec::decode_inert(active.bytes())?;
    let (old_account, new_account) = replay_account_fixtures()?;
    assert_eq!(artifact.binding().account(), old_account);
    let (_, _, model) = target_values()?;
    let source = replay::exact_replay_for_kind(
        request.input().items().get(1),
        ResponsesReplayKind::new("openai.output_item", 2),
    )?
    .ok_or("replay missing")?;
    for scope in [
        ReplayScope::ExactAccountModel {
            model: model.clone(),
            account: new_account,
        },
        ReplayScope::ExactModel(model),
    ] {
        let rejected = ResponsesArtifact::new(
            artifact.binding().clone(),
            artifact.production().clone(),
            artifact.replay_contract().clone(),
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                replay_with_scope(source, scope)?,
            )],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            Timestamp::new(1_700_000_000, 0)?,
        );
        assert_eq!(
            rejected.err(),
            Some(plakous_responses::ResponsesArtifactError::ReplayBindingMismatch { index: 0 })
        );
    }
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture distinguishes append integrity, covered-window account validation and fresh lowering"
)]
fn foreign_account_or_accountless_covered_output_prepares_fresh() -> TestResult {
    let (base, _) = active_replay_fixture(ResponsesStorageMode::Stateless, None)?;
    let (protocol, family, model) = target_values()?;
    let (old_account, new_account) = replay_account_fixtures()?;
    let kind = ResponsesReplayKind::new("openai.output_item", 2);
    let source = replay::exact_replay_for_kind(base.input().items().get(1), kind)?
        .ok_or("replay missing")?;
    let prefix = base.input().history().segments()[0].clone();
    let producing = ModelRequest::from_parts(
        base.stable_prefix().clone(),
        ConversationView::from_shared(
            CanonicalHistory::new(vec![prefix])?,
            WorkingProjection::default(),
        ),
        metadata()?,
    )?;
    let now = Timestamp::new(1_700_000_000, 0)?;
    for scope in [
        ReplayScope::ExactAccountModel {
            model: model.clone(),
            account: new_account,
        },
        ReplayScope::ExactModel(model.clone()),
    ] {
        let mut segments = base.input().history().segments().to_vec();
        segments[1] = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                id::<EntryId>(501)?,
                Actor::Assistant,
                vec![ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::OpaqueReplay,
                    None,
                    Some(replay_with_scope(source, scope)?),
                )?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let request = ModelRequest::from_parts(
            base.stable_prefix().clone(),
            ConversationView::from_shared(
                CanonicalHistory::new(segments)?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )?;
        // Seal the malformed canonical window but keep a valid append envelope,
        // so canonical authorization must independently reject its account.
        let artifact = output_artifact(
            &producing,
            &request,
            "responses-test-route",
            vec![source.clone()],
            ResponsesStorageMode::Stateless,
            "unused",
            None,
            now,
            b"bad-covered-account",
        )?;
        let authority = operation_authority(
            &request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?;
        assert_eq!(authority.artifact_binding().account(), old_account);
        let error = ResponsesArtifactReplayBridge::from_active_artifact(
            &active_artifact(&artifact)?,
            &request,
            authority,
            kind,
            None,
            None,
            now,
        )
        .err()
        .ok_or("covered account mismatch was admitted")?;
        assert!(
            matches!(
                error,
                ResponsesRequestError::Replay(ResponsesReplayError::ArtifactCanonicalMismatch)
            ),
            "{error:?}"
        );
        assert!(error.artifact_is_not_for_this_request());
        let fresh = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        assert!(
            !fresh.body()["input"]
                .to_string()
                .contains("fixture-encrypted")
        );
    }
    Ok(())
}

#[test]
fn hosted_programs_ignore_lineage_from_an_earlier_account() -> TestResult {
    let (protocol, family, model) = target_values()?;
    let (old_account, new_account) = replay_account_fixtures()?;
    let envelope = ReplayEnvelope::new(protocol, Some(family), ReplayScope::ExactAccountModel { model, account: old_account },
        ReplayKindId::new("openai.output_item")?, 2, DataClassification::Sensitive,
        RawValue::from_string(json!({"type":"program", "id":"old-program", "call_id":"old-call", "code":"test", "fingerprint":"old-fingerprint"}).to_string())?)?;
    let request = crate::test_support::model_request_builder(
        ConversationView::from_items(vec![ConversationItem::new(
            id::<EntryId>(41_000)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                envelope,
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?])?,
        metadata()?,
    )
    .resolved_harness_plan_identity(harness_plan_identity()?)
    .build()?;
    let old_authority = operation_authority(
        &request,
        "responses-test-route",
        ResponsesStoragePolicy::Stateless,
    )?;
    let descriptor = descriptor("responses-test-route")?;
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let new_authority = ResponsesRequestAuthority::from_admitted_operation(
        &descriptor,
        &plan,
        &request,
        new_account,
        ResponsesSemanticRevisions::new(1, 1, 1)?,
        None,
        ResponsesStoragePolicy::Stateless,
    )?;
    let kind = ResponsesReplayKind::new("openai.output_item", 2);
    let same = replay::validated_hosted_programs(&request, 1, kind, &old_authority)?;
    assert_eq!(
        same.live_program("old-call"),
        Some(("old-program", "old-fingerprint"))
    );
    let foreign = replay::validated_hosted_programs(&request, 1, kind, &new_authority)?;
    assert_eq!(foreign, crate::ResponsesHostedProgramContext::default());
    Ok(())
}
