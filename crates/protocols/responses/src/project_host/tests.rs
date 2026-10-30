use crate::response_host::parse_output_item;
use std::collections::BTreeMap;

use super::*;
use crate::response_host::{
    ResponsesHostDecodeError, ResponsesResponseLimits, ResponsesUsagePolicy,
};
use crate::{MAX_RESPONSES_ARTIFACT_BYTES, ResponsesSemanticRevisions};
use plakous_model::{
    Actor, AttemptId, CallId, Capability, CapabilityConstraints, CapabilitySet, EntryId,
    FactProvenance, FactSource, FeatureSupport, McpServerId, ModelAdapterProfileId, ModelResponse,
    NamespaceId, OpaqueBlock, OpaqueFallback, Provenance, ReasoningBlock, RequestMetadata,
    ResolvedModelDescriptor, RouteId, RunId, Support, TextBlock, Timestamp, ToolConcurrency,
    ToolCorrelation, ToolEffect, ToolInputContract, ToolName, ToolOutputContract, ToolResultBlock,
    ToolResultKind, ToolResultPart, TurnId, ValidationMode,
};
use serde_json::json;
use uuid::Uuid;

fn replay() -> ResponsesReplayProjection {
    let Ok(protocol) = ProtocolId::new("openai.responses") else {
        panic!("valid protocol test fixture");
    };
    let Ok(model) = ModelId::new("gpt-test") else {
        panic!("valid model test fixture");
    };
    let Ok(output_kind) = ReplayKindId::new("responses.output_item") else {
        panic!("valid output kind test fixture");
    };
    let Ok(replay) = ResponsesReplayProjection::new(
        protocol,
        None,
        model,
        output_kind,
        1,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"project-host-account"])
            .expect("account binding"),
    ) else {
        panic!("valid replay test fixture");
    };
    replay
}

fn provider_request() -> ResponsesProviderRequestBinding {
    ResponsesProviderRequestBinding::new(
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"project-host-physical-request"],
        )
        .expect("physical request digest"),
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"project-host-terminal-response"],
        )
        .expect("terminal response digest"),
        None,
    )
    .expect("provider request binding")
}

#[test]
fn strict_reasoning_projection_requires_completed_nonempty_encrypted_content() {
    let strict = replay().requiring_encrypted_reasoning();
    assert_ne!(
        replay().execution_identity().expect("ordinary identity"),
        strict.execution_identity().expect("strict identity")
    );
    for reasoning in [
        json!({"id":"r1","type":"reasoning","status":"completed","summary":[]}),
        json!({
            "id":"r1","type":"reasoning","status":"completed","summary":[],
            "encrypted_content":""
        }),
        json!({
            "id":"r1","type":"reasoning","status":"in_progress","summary":[],
            "encrypted_content":"opaque"
        }),
    ] {
        let response = ResponsesResponse::parse(
            &json!({
                "id":"resp-strict","object":"response","status":"completed",
                "model":"gpt-test","output":[reasoning]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("response");
        assert!(matches!(
            project_terminal_response(
                response,
                &strict,
                ResponsesStorageMode::Stateless,
                provider_request(),
            ),
            Err(ResponsesProjectError::InvalidTerminal(
                "strict reasoning replay is incomplete or lacks encrypted content"
            ))
        ));
    }

    let response = ResponsesResponse::parse(
        &json!({
            "id":"resp-strict","object":"response","status":"completed",
            "model":"gpt-test",
            "output":[{
                "id":"r1","type":"reasoning","status":"completed","summary":[],
                "encrypted_content":"opaque"
            }]
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )
    .expect("response");
    assert!(
        project_terminal_response(
            response,
            &strict,
            ResponsesStorageMode::Stateless,
            provider_request(),
        )
        .is_ok()
    );
}

#[test]
fn codex_reasoning_projection_requires_encrypted_content_with_optional_status() {
    let codex = replay().requiring_encrypted_reasoning_with_optional_status();
    for (reasoning, accepted) in [
        (
            json!({
                "id":"r1","type":"reasoning","summary":[],
                "encrypted_content":"opaque"
            }),
            true,
        ),
        (
            json!({
                "id":"r1","type":"reasoning","status":"completed","summary":[],
                "encrypted_content":"opaque"
            }),
            true,
        ),
        (json!({"id":"r1","type":"reasoning","summary":[]}), false),
        (
            json!({
                "id":"r1","type":"reasoning","status":"in_progress","summary":[],
                "encrypted_content":"opaque"
            }),
            false,
        ),
    ] {
        let response = ResponsesResponse::parse(
            &json!({
                "id":"resp-codex","object":"response","status":"completed",
                "model":"gpt-test","output":[reasoning]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("response");
        assert_eq!(
            project_terminal_response(
                response,
                &codex,
                ResponsesStorageMode::Stateless,
                provider_request(),
            )
            .is_ok(),
            accepted
        );
    }
}

fn request_with_tool(custom: bool) -> ModelRequest {
    let input = if custom {
        ToolInputContract::utf8(65_536).expect("UTF-8 contract")
    } else {
        ToolInputContract::structured(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false})).expect("schema")
    };
    crate::test_support::model_request_builder(
        plakous_model::ConversationView::default(),
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1)).expect("run"),
            TurnId::try_from(Uuid::from_u128(2)).expect("turn"),
            AttemptId::try_from(Uuid::from_u128(3)).expect("attempt"),
            Timestamp::new(1_700_000_000, 0).expect("time"),
        ),
    )
    .resolved_harness_plan_identity(
        Digest::sha256(
            DigestDomain::ResolvedHarnessPlan,
            1,
            &[b"project-host-harness-plan"],
        )
        .expect("harness plan identity"),
    )
    .tools(vec![plakous_model::ConfiguredTool::new(
        plakous_model::ToolDefinition::new(
            ToolName::new("lookup").expect("tool"),
            "1.0.0",
            "lookup",
            input,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )
        .expect("definition"),
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectOnly,
    )])
    .build()
    .expect("request")
}

#[test]
fn projected_function_kind_uses_exposed_request_authority() {
    let base = request_with_tool(true);
    let schema = plakous_model::PortableToolSchemaV2::new(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "properties":{"command":{"type":"string"}},
        "required":["command"],
        "additionalProperties":false
    }))
    .expect("schema");
    let identity = Digest::sha256(DigestDomain::ToolDefinition, 2, &[b"projected-function"])
        .expect("identity");
    let target = plakous_model::ToolProjectionTargetV2::new(
        plakous_model::IntegrationClass::Standard,
        "xai.responses.grok-subscription.2026-07",
        "xai.grok_subscription",
        "xai.grok.compat.2026-07-31.r1",
        "responses-family",
        "grok-4.5",
        "grok-4.5",
        base.effective_provider_configuration_id(),
        4,
    )
    .expect("target");
    let canonical = base.tools()[0].definition().name().clone();
    let projection = plakous_model::CompiledToolProjectionV2::new(
        target,
        identity,
        identity,
        vec![
            plakous_model::ProjectedToolDefinitionV2::new(
                canonical.clone(),
                vec![canonical],
                ToolName::new("run_terminal_cmd").expect("exposed name"),
                "projected function",
                schema.clone(),
                schema,
                identity,
            )
            .expect("definition"),
        ],
    )
    .expect("projection");
    let prefix = base
        .stable_prefix()
        .with_tool_projection(projection)
        .expect("prefix");
    let request =
        ModelRequest::from_parts(prefix, base.input().clone(), base.metadata()).expect("request");
    assert!(matches!(
        request_tool_kind(&request, "run_terminal_cmd"),
        Some(RawToolCallKind::Function)
    ));
}

#[test]
fn projected_custom_kind_uses_explicit_presentation_not_schema_inference() {
    let base = request_with_tool(true);
    let output = plakous_model::PortableToolSchemaV2::new(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object", "properties":{}, "required":[],
        "additionalProperties":false
    }))
    .expect("schema");
    let identity =
        Digest::sha256(DigestDomain::ToolDefinition, 3, &[b"projected-custom"]).expect("identity");
    let target = plakous_model::ToolProjectionTargetV2::new(
        plakous_model::IntegrationClass::Standard,
        "openai.codex.responses",
        "openai.codex_subscription",
        "openai.codex.compat.r1",
        "responses-family",
        "gpt-codex",
        "gpt-codex",
        base.effective_provider_configuration_id(),
        5,
    )
    .expect("target");
    let canonical = base.tools()[0].definition().name().clone();
    let projection = plakous_model::CompiledToolProjectionV2::new(
        target,
        identity,
        identity,
        vec![
            plakous_model::ProjectedToolDefinitionV2::new_custom(
                canonical.clone(),
                vec![canonical],
                ToolName::new("exec").expect("exposed name"),
                "execute code",
                plakous_model::Utf8TextContractV1::new(65_536).expect("bound"),
                plakous_model::ProjectedCustomToolFormatV2::lark("start: /(?s).+/")
                    .expect("grammar"),
                output,
                identity,
            )
            .expect("definition"),
        ],
    )
    .expect("projection");
    let prefix = base
        .stable_prefix()
        .with_tool_projection(projection)
        .expect("prefix");
    let request =
        ModelRequest::from_parts(prefix, base.input().clone(), base.metadata()).expect("request");
    assert!(matches!(
        request_tool_kind(&request, "exec"),
        Some(RawToolCallKind::Custom)
    ));
}

fn descriptor() -> ResolvedModelDescriptor {
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(1_700_000_000, 0).expect("time"),
        None,
    );
    let mut capabilities = BTreeMap::new();
    for capability in [Capability::Streaming, Capability::Tools] {
        capabilities.insert(
            capability,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
    }
    ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("openai.responses").expect("protocol"),
        RouteId::new("responses-test").expect("route"),
        ModelId::new("gpt-test").expect("model"),
        None,
        None,
        ModelAdapterProfileId::new("responses-test-adapter").expect("adapter"),
        CapabilitySet::new(capabilities, BTreeMap::new()),
    )
}

fn admitted_plan(request: &ModelRequest) -> ValidatedRequestPlan {
    descriptor()
        .validate_request(request.clone(), ValidationMode::Strict)
        .expect("admitted plan")
}

fn artifact_binding(plan: &ValidatedRequestPlan) -> ResponsesArtifactBinding {
    ResponsesArtifactBinding::from_admitted_operation(
        &descriptor(),
        plan,
        plan.request(),
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"project-host-account"])
            .expect("account binding"),
        ResponsesSemanticRevisions::new(1, 1, 3).expect("semantic revisions"),
        Some(
            Digest::sha256(
                DigestDomain::CacheAffinity,
                1,
                &[b"project-host-cache-affinity"],
            )
            .expect("cache affinity"),
        ),
    )
    .expect("artifact binding")
}

fn sealed_artifact_fixture() -> (
    ResponsesSealedArtifactCandidate,
    ValidatedRequestPlan,
    CanonicalItemPrefixIdentity,
    Vec<ConversationItem>,
) {
    let response = ResponsesResponse::parse(
        &json!({
            "id":"resp_neutral_secret","object":"response","status":"completed",
            "model":"gpt-test",
            "output":[{
                "id":"m1","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"sealed-secret","annotations":[]}]
            }]
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )
    .expect("response");
    let projected = project_terminal_response(
        response,
        &replay(),
        ResponsesStorageMode::ProviderStored,
        provider_request(),
    )
    .expect("projection");
    let output_replay = projected.artifact_candidate().expect("candidate").replay()[0]
        .envelope()
        .clone();
    let canonical = item(
        75,
        vec![
            replay_block(output_replay),
            ContentBlock::Text(
                TextBlock::new("sealed-secret", Some(TextPhase::Final)).expect("text"),
            ),
        ],
    );
    let request = request_with_tool(false);
    let plan = admitted_plan(&request);
    let paired = projected
        .pair_canonical(&request, vec![canonical.clone()])
        .expect("paired");
    let (_, _, _, Some(candidate)) = paired.into_parts() else {
        panic!("paired artifact candidate");
    };
    let covered = plakous_model::ConversationView::from_items(vec![canonical.clone()])
        .expect("test conversation items form one valid segment")
        .canonical_item_prefix_identity(1)
        .expect("covered output");
    (
        candidate.seal(&plan, covered).expect("sealed candidate"),
        plan,
        covered,
        vec![canonical],
    )
}

fn item(value: u128, blocks: Vec<ContentBlock>) -> ConversationItem {
    ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(value)).expect("entry"),
        Actor::Assistant,
        blocks,
        Provenance::ModelOutput,
    )
    .expect("item")
}

fn raw_item(value: serde_json::Value) -> ResponsesOutputItem {
    parse_output_item(&value).expect("raw item")
}

fn envelope(value: &serde_json::Value) -> ReplayEnvelope {
    replay_envelope(
        value,
        &ReplayKindId::new("responses.output_item").expect("kind"),
        1,
        &replay(),
    )
    .expect("envelope")
}

fn replay_block(replay: ReplayEnvelope) -> ContentBlock {
    ContentBlock::Opaque(
        OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay).expect("opaque replay"),
    )
}

fn call_block(
    owner: ToolExecutionOwner,
    state: ToolCallState,
    remote_id: &str,
    name: &str,
    arguments: &str,
) -> ContentBlock {
    ContentBlock::ToolCall(
        plakous_model::ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(10)).expect("call"),
            ToolName::new(name).expect("tool"),
            owner,
            state,
            ToolCorrelation::new(
                Some(remote_id.to_owned()),
                None,
                plakous_model::ToolCallerLineage::Direct,
            )
            .expect("correlation"),
            arguments,
        )
        .expect("tool call"),
    )
}

#[test]
fn replay_projection_matches_only_its_exact_target() {
    let projection = replay();
    let protocol = ProtocolId::new("openai.responses").expect("protocol");
    let model = ModelId::new("gpt-test").expect("model");
    assert!(projection.matches_target(&protocol, None, &model, projection.account));

    let other_protocol = ProtocolId::new("xai.responses").expect("other protocol");
    let other_model = ModelId::new("grok-test").expect("other model");
    let family = ModelFamilyId::new("gpt-family").expect("family");
    assert!(!projection.matches_target(&other_protocol, None, &model, projection.account));
    assert!(!projection.matches_target(&protocol, None, &other_model, projection.account));
    assert!(!projection.matches_target(&protocol, Some(&family), &model, projection.account));

    assert!(!projection.matches_target(
        &protocol,
        None,
        &model,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"other-account"]).expect("account")
    ));

    let ambiguous = projection
        .with_opaque_output_kind(
            ReplayKindId::new("responses.output_item").expect("same kind"),
            2,
        )
        .expect("projection shape");
    assert!(ambiguous.artifact_replay_contract().is_err());
}

/// The raw replay item pairs with the name the model called: a call whose
/// canonical name differs replays under its exposed name.
#[test]
fn raw_call_pairing_binds_the_exposed_name_not_the_canonical_name() {
    let native = json!({
        "id":"c1","type":"function_call","status":"completed",
        "call_id":"remote-1","name":"lookup","arguments":"{}"
    });
    let raw = raw_item(native.clone());
    let replay = envelope(&native);
    let request = request_with_tool(false);
    let aliased = |exposed: &str, canonical: &str| {
        let ContentBlock::ToolCall(call) = call_block(
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            "remote-1",
            exposed,
            "{}",
        ) else {
            unreachable!("call_block builds a tool call");
        };
        ContentBlock::ToolCall(
            call.with_canonical_name(ToolName::new(canonical).expect("canonical")),
        )
    };
    let paired = item(
        11,
        vec![replay_block(replay.clone()), aliased("lookup", "search")],
    );
    assert!(validate_output_semantic_pair(&raw, &replay, &paired, &request).is_ok());
    let canonical_only = item(
        12,
        vec![replay_block(replay.clone()), aliased("search", "lookup")],
    );
    assert!(validate_output_semantic_pair(&raw, &replay, &canonical_only, &request).is_err());
}

#[test]
fn raw_call_pairing_accepts_mcp_and_rejects_every_execution_mutation() {
    let native = json!({
        "id":"c1","type":"function_call","status":"completed",
        "call_id":"remote-1","name":"lookup","arguments":"{}"
    });
    let raw = raw_item(native.clone());
    let replay = envelope(&native);
    let request = request_with_tool(false);
    let mcp = ToolExecutionOwner::Mcp {
        server: McpServerId::new("test-mcp").expect("server"),
    };
    let canonical = item(
        11,
        vec![
            replay_block(replay.clone()),
            call_block(mcp, ToolCallState::Requested, "remote-1", "lookup", "{}"),
        ],
    );
    assert!(validate_output_semantic_pair(&raw, &replay, &canonical, &request).is_ok());

    let mutations = [
        call_block(
            ToolExecutionOwner::ProviderHosted {
                provider_kind: NamespaceId::new("provider.lookup").expect("kind"),
            },
            ToolCallState::Requested,
            "remote-1",
            "lookup",
            "{}",
        ),
        call_block(
            ToolExecutionOwner::Local,
            ToolCallState::Pending,
            "remote-1",
            "lookup",
            "{}",
        ),
        call_block(
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            "wrong-id",
            "lookup",
            "{}",
        ),
        call_block(
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            "remote-1",
            "other",
            "{}",
        ),
        call_block(
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            "remote-1",
            "lookup",
            "{\"wrong\":true}",
        ),
    ];
    for (index, mutation) in mutations.into_iter().enumerate() {
        let candidate = item(
            20 + u128::try_from(index).expect("index"),
            vec![replay_block(replay.clone()), mutation],
        );
        assert!(
            validate_output_semantic_pair(&raw, &replay, &candidate, &request).is_err(),
            "call mutation {index} was accepted"
        );
    }
    let extra = item(
        30,
        vec![
            replay_block(replay.clone()),
            call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                "remote-1",
                "lookup",
                "{}",
            ),
            ContentBlock::Text(TextBlock::new("invented", Some(TextPhase::Final)).expect("text")),
        ],
    );
    assert!(validate_output_semantic_pair(&raw, &replay, &extra, &request).is_err());

    let extra_result = item(
        31,
        vec![
            replay_block(replay.clone()),
            call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                "remote-1",
                "lookup",
                "{}",
            ),
            ContentBlock::ToolResult(
                ToolResultBlock::new(
                    CallId::try_from(Uuid::from_u128(10)).expect("call"),
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(
                        TextBlock::new("result", None).expect("text"),
                    )],
                    ToolCorrelation::new(
                        Some("remote-1".to_owned()),
                        None,
                        plakous_model::ToolCallerLineage::Direct,
                    )
                    .expect("correlation"),
                )
                .expect("result"),
            ),
        ],
    );
    assert!(validate_output_semantic_pair(&raw, &replay, &extra_result, &request).is_err());

    let custom_native = json!({
        "id":"c2","type":"custom_tool_call","status":"completed",
        "call_id":"remote-2","name":"lookup","input":"plain text"
    });
    let custom = raw_item(custom_native.clone());
    let custom_replay = envelope(&custom_native);
    let custom_canonical = item(
        32,
        vec![
            replay_block(custom_replay.clone()),
            call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                "remote-2",
                "lookup",
                "\"plain text\"",
            ),
        ],
    );
    assert!(
        validate_output_semantic_pair(
            &custom,
            &custom_replay,
            &custom_canonical,
            &request_with_tool(true),
        )
        .is_ok()
    );
    assert!(
        validate_output_semantic_pair(&custom, &custom_replay, &custom_canonical, &request,)
            .is_err()
    );

    let empty_native = json!({
        "id":"c3","type":"custom_tool_call","status":"completed",
        "call_id":"remote-3","name":"lookup","input":""
    });
    let empty = raw_item(empty_native.clone());
    let empty_replay = envelope(&empty_native);
    let empty_canonical = item(
        33,
        vec![
            replay_block(empty_replay.clone()),
            call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                "remote-3",
                "lookup",
                "\"\"",
            ),
        ],
    );
    assert!(
        validate_output_semantic_pair(
            &empty,
            &empty_replay,
            &empty_canonical,
            &request_with_tool(true),
        )
        .is_ok()
    );

    for invalid in ["", "not json", "{\"key\":1,\"key\":2}", "[]"] {
        let invalid_native = json!({
            "id":"f-invalid","type":"function_call","status":"completed",
            "call_id":"remote-invalid","name":"lookup","arguments":invalid
        });
        let invalid_raw = raw_item(invalid_native.clone());
        let invalid_replay = envelope(&invalid_native);
        let canonical_arguments = if invalid.is_empty() { "null" } else { invalid };
        let invalid_canonical = item(
            34,
            vec![
                replay_block(invalid_replay.clone()),
                call_block(
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    "remote-invalid",
                    "lookup",
                    canonical_arguments,
                ),
            ],
        );
        assert!(
            validate_output_semantic_pair(
                &invalid_raw,
                &invalid_replay,
                &invalid_canonical,
                &request,
            )
            .is_err(),
            "invalid function arguments were accepted: {invalid:?}"
        );
    }
}

#[test]
fn message_reasoning_and_opaque_semantics_are_exact() {
    let request = request_with_tool(false);
    let message_native = json!({
        "id":"m1","type":"message","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":"exact","annotations":[]}]
    });
    let message = raw_item(message_native.clone());
    let message_replay = envelope(&message_native);
    let exact_message = item(
        40,
        vec![
            replay_block(message_replay.clone()),
            ContentBlock::Text(TextBlock::new("exact", Some(TextPhase::Final)).expect("text")),
        ],
    );
    assert!(
        validate_output_semantic_pair(&message, &message_replay, &exact_message, &request).is_ok()
    );
    for (index, block) in [
        ContentBlock::Text(TextBlock::new("tampered", Some(TextPhase::Final)).expect("text")),
        ContentBlock::Text(TextBlock::new("exact", Some(TextPhase::Commentary)).expect("text")),
    ]
    .into_iter()
    .enumerate()
    {
        let candidate = item(
            41 + u128::try_from(index).expect("index"),
            vec![replay_block(message_replay.clone()), block],
        );
        assert!(
            validate_output_semantic_pair(&message, &message_replay, &candidate, &request).is_err()
        );
    }

    let commentary_native = json!({
        "id":"m2","type":"message","role":"assistant","status":"completed",
        "phase":"commentary",
        "content":[{"type":"output_text","text":"working","annotations":[]}]
    });
    let commentary = raw_item(commentary_native.clone());
    let commentary_replay = envelope(&commentary_native);
    let exact_commentary = item(
        43,
        vec![
            replay_block(commentary_replay.clone()),
            ContentBlock::Text(
                TextBlock::new("working", Some(TextPhase::Commentary)).expect("commentary"),
            ),
        ],
    );
    assert!(
        validate_output_semantic_pair(
            &commentary,
            &commentary_replay,
            &exact_commentary,
            &request,
        )
        .is_ok()
    );

    for (actor, provenance) in [
        (Actor::User, Provenance::ModelOutput),
        (Actor::Assistant, Provenance::Imported),
    ] {
        let invalid_authority = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(44)).expect("entry"),
            actor,
            exact_commentary.blocks().to_vec(),
            provenance,
        )
        .expect("structurally valid item");
        assert!(
            validate_output_semantic_pair(
                &commentary,
                &commentary_replay,
                &invalid_authority,
                &request,
            )
            .is_err()
        );
    }

    let reasoning_native = json!({
        "id":"r1","type":"reasoning","status":"completed",
        "summary":[{"type":"summary_text","text":"summary"}],
        "encrypted_content":"opaque"
    });
    let reasoning = raw_item(reasoning_native.clone());
    let reasoning_replay = envelope(&reasoning_native);
    let exact_reasoning = item(
        50,
        vec![
            replay_block(reasoning_replay.clone()),
            ContentBlock::Reasoning(
                ReasoningBlock::new(
                    ReasoningVisibility::Summary,
                    Some("summary".to_owned()),
                    None,
                )
                .expect("reasoning"),
            ),
        ],
    );
    assert!(
        validate_output_semantic_pair(&reasoning, &reasoning_replay, &exact_reasoning, &request)
            .is_ok()
    );
    let visible = item(
        51,
        vec![
            replay_block(reasoning_replay.clone()),
            ContentBlock::Reasoning(
                ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some("summary".to_owned()),
                    None,
                )
                .expect("reasoning"),
            ),
        ],
    );
    assert!(
        validate_output_semantic_pair(&reasoning, &reasoning_replay, &visible, &request).is_err()
    );

    let opaque_native = json!({"id":"u1","type":"future_item","status":"completed"});
    let opaque = raw_item(opaque_native.clone());
    let opaque_replay = envelope(&opaque_native);
    let invented = item(
        60,
        vec![
            replay_block(opaque_replay.clone()),
            ContentBlock::Text(TextBlock::new("invented", Some(TextPhase::Final)).expect("text")),
        ],
    );
    assert!(validate_output_semantic_pair(&opaque, &opaque_replay, &invented, &request).is_err());
}

#[test]
fn pair_canonical_requires_exact_replay_before_atomic_promotion() {
    let response = ResponsesResponse::parse(
        &json!({
            "id":"resp_pair","object":"response","status":"completed","model":"gpt-test",
            "output":[{"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"exact","annotations":[]}]}]
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )
    .expect("response");
    let projected = project_terminal_response(
        response,
        &replay(),
        ResponsesStorageMode::Stateless,
        provider_request(),
    )
    .expect("projection");
    let replay = projected.artifact_candidate().expect("candidate").replay()[0]
        .envelope()
        .clone();
    let canonical = item(
        70,
        vec![
            replay_block(replay),
            ContentBlock::Text(TextBlock::new("exact", Some(TextPhase::Final)).expect("text")),
        ],
    );
    assert!(
        projected
            .clone()
            .pair_canonical(&request_with_tool(false), vec![canonical])
            .is_ok()
    );
    let missing_replay = item(
        71,
        vec![ContentBlock::Text(
            TextBlock::new("exact", Some(TextPhase::Final)).expect("text"),
        )],
    );
    assert!(
        projected
            .pair_canonical(&request_with_tool(false), vec![missing_replay])
            .is_err()
    );
}

#[test]
fn artifact_seal_requires_runtime_derived_exact_coverage() {
    let response = ResponsesResponse::parse(
        &json!({
            "id":"resp_seal","object":"response","status":"completed","model":"gpt-test",
            "output":[{"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"sealed","annotations":[]}]}]
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )
    .expect("response");
    let projected = project_terminal_response(
        response,
        &replay(),
        ResponsesStorageMode::ProviderStored,
        provider_request(),
    )
    .expect("projection");
    let replay = projected.artifact_candidate().expect("candidate").replay()[0]
        .envelope()
        .clone();
    let canonical = item(
        72,
        vec![
            replay_block(replay),
            ContentBlock::Text(TextBlock::new("sealed", Some(TextPhase::Final)).expect("text")),
        ],
    );
    let request = request_with_tool(false);
    let plan = admitted_plan(&request);
    let paired = projected
        .pair_canonical(&request, vec![canonical.clone()])
        .expect("paired");
    let (_, _, _, Some(candidate)) = paired.into_parts() else {
        panic!("paired artifact candidate");
    };
    let wrong_coverage = plakous_model::ConversationView::default()
        .canonical_item_prefix_identity(0)
        .expect("empty coverage");
    assert!(matches!(
        candidate.clone().seal(&plan, wrong_coverage),
        Err(ResponsesProjectError::InvalidArtifactCoverage)
    ));

    let covered = plakous_model::ConversationView::from_items(vec![canonical])
        .expect("test conversation items form one valid segment")
        .canonical_item_prefix_identity(1)
        .expect("covered output");
    let sealed = candidate.seal(&plan, covered).expect("sealed candidate");
    let (_, production, contract, replay, continuation, storage) = sealed.into_parts();
    assert_eq!(production.output_items(), 1);
    assert_eq!(production.producing_projection_items(), 0);
    assert_eq!(production.covered_projection_items(), 1);
    assert_eq!(production.covered_projection(), covered.identity());
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].output_index(), 0);
    assert_eq!(storage, ResponsesStorageMode::ProviderStored);
    let continuation = continuation.expect("provider continuation");
    assert_eq!(continuation.response_id(), "resp_seal");
    assert_eq!(continuation.covered_projection_items(), 1);
    assert_eq!(continuation.covered_projection(), covered.identity());
    assert_eq!(
        continuation.terminal_response(),
        production.terminal_response()
    );
    assert_eq!(
        contract
            .for_purpose(ResponsesReplayPurpose::OutputItem)
            .map(|kind| (kind.kind().as_str(), kind.version())),
        Some(("responses.output_item", 1))
    );
}

#[test]
fn neutral_candidate_is_exact_v3_and_validates_every_producer_fact() {
    let (sealed, plan, covered, output) = sealed_artifact_fixture();
    let binding = artifact_binding(&plan);
    let now = Timestamp::new(1_700_000_100, 0).expect("time");
    let candidate = seal_responses_family_artifact(sealed, &plan, covered, binding.clone(), now)
        .expect("neutral candidate");

    assert_eq!(candidate.family(), "responses");
    assert_eq!(
        candidate.schema_revision(),
        RESPONSES_ARTIFACT_SCHEMA_REVISION
    );
    assert_eq!(
        candidate.codec_revision(),
        RESPONSES_ARTIFACT_CODEC_REVISION
    );
    assert_eq!(candidate.binding(), binding.identity());
    assert!(candidate.bytes().len() <= MAX_RESPONSES_ARTIFACT_BYTES);
    candidate
        .validate_for_commit(&plan, covered, output.len())
        .expect("exact producer validation");

    let artifact =
        ResponsesArtifactCodec::decode_inert(candidate.bytes()).expect("inert family decode");
    let producer = candidate.producer();
    assert_eq!(producer.request(), artifact.production().producer_request());
    assert_eq!(producer.model_input(), artifact.binding().model_input());
    assert_eq!(producer.plan(), artifact.production().validated_plan());
    assert_eq!(
        producer.cache_affinity(),
        artifact.binding().cache_affinity()
    );
    assert_eq!(producer.run(), artifact.binding().run());
    assert_eq!(producer.turn(), artifact.binding().turn());
    assert_eq!(producer.attempt(), artifact.binding().attempt());
    assert_eq!(
        producer.source_projection(),
        artifact.production().producing_projection()
    );
    assert_eq!(
        producer.source_projection_items(),
        artifact.production().producing_projection_items()
    );
    assert_eq!(
        producer.output_items(),
        artifact.production().output_items()
    );
    assert_eq!(
        producer.covered_projection(),
        artifact.production().covered_projection()
    );
    assert_eq!(
        producer.covered_projection_items(),
        artifact.production().covered_projection_items()
    );
    assert_eq!(
        producer.physical_operation(),
        artifact.production().physical_request()
    );
    assert_eq!(
        producer.terminal_operation(),
        artifact.production().terminal_response()
    );
    assert_eq!(
        artifact.schema_revision(),
        RESPONSES_ARTIFACT_SCHEMA_REVISION
    );
    artifact
        .validate_for(&binding, artifact.production(), now)
        .expect("exact family producer validation");

    let debug = format!("{candidate:?}");
    assert!(debug.contains("<redacted;"));
    assert!(!debug.contains("resp_neutral_secret"));
    assert!(!debug.contains("sealed-secret"));
}

#[test]
fn neutral_candidate_rejects_foreign_facts_and_corruption() {
    let (sealed, plan, covered, output) = sealed_artifact_fixture();
    let binding = artifact_binding(&plan);
    let now = Timestamp::new(1_700_000_100, 0).expect("time");

    let foreign_plan = admitted_plan(&request_with_tool(true));
    assert!(matches!(
        seal_responses_family_artifact(
            sealed.clone(),
            &foreign_plan,
            covered,
            binding.clone(),
            now,
        ),
        Err(ResponsesProjectError::FamilyArtifact(
            plakous_model::FamilyArtifactError::ForeignProducerFacts
        ))
    ));

    let foreign_covered = plakous_model::ConversationView::from_items(vec![item(
        76,
        vec![ContentBlock::Text(
            TextBlock::new("foreign", Some(TextPhase::Final)).expect("text"),
        )],
    )])
    .expect("test conversation items form one valid segment")
    .canonical_item_prefix_identity(1)
    .expect("foreign covered output");
    assert!(matches!(
        seal_responses_family_artifact(
            sealed.clone(),
            &plan,
            foreign_covered,
            binding.clone(),
            now,
        ),
        Err(ResponsesProjectError::FamilyArtifact(
            plakous_model::FamilyArtifactError::ForeignProducerFacts
        ))
    ));

    let candidate = seal_responses_family_artifact(sealed, &plan, covered, binding, now)
        .expect("neutral candidate");
    assert!(matches!(
        candidate.validate_for_commit(&plan, covered, output.len() + 1),
        Err(plakous_model::FamilyArtifactError::ResponseOutputMismatch)
    ));
    let mut corrupted = candidate.bytes().to_vec();
    let last = corrupted.last_mut().expect("encoded artifact byte");
    *last ^= 1;
    assert!(matches!(
        ResponsesArtifactCodec::decode_inert(&corrupted),
        Err(ResponsesArtifactError::IntegrityMismatch)
    ));
}

#[test]
fn neutral_artifact_bytes_never_enter_response_replay_or_canonical_output() {
    let (sealed, plan, covered, output) = sealed_artifact_fixture();
    let candidate = seal_responses_family_artifact(
        sealed,
        &plan,
        covered,
        artifact_binding(&plan),
        Timestamp::new(1_700_000_100, 0).expect("time"),
    )
    .expect("neutral candidate");
    assert!(candidate.bytes().starts_with(b"PLAKRSP\0"));

    let response = ModelResponse::new(
        output,
        FinishDetails::new(FinishReason::Stop, None),
        None,
        Some("resp_neutral_secret".to_owned()),
        plan.digest(),
    )
    .with_family_artifact_candidate(candidate)
    .expect("sidecar attachment");
    assert!(
        !serde_json::to_value(&response)
            .expect("response JSON")
            .as_object()
            .is_some_and(|object| object.contains_key("replay"))
    );
    let canonical = serde_json::to_vec(response.output()).expect("canonical JSON");
    assert!(
        !canonical
            .windows(b"PLAKRSP".len())
            .any(|part| part == b"PLAKRSP")
    );
    assert!(
        !canonical
            .windows(b"responses.artifact".len())
            .any(|part| { part == b"responses.artifact" })
    );
    assert!(
        !canonical
            .windows(b"\"encoding\":\"base64\"".len())
            .any(|part| { part == b"\"encoding\":\"base64\"" })
    );
}

#[test]
fn explicit_end_turn_false_projects_continuation_with_tool_and_refusal_precedence() {
    let text = json!({"id":"m1","type":"message","role":"assistant", "status":"completed",
        "content":[{"type":"output_text","text":"continuing","annotations":[]}]});
    for (signal, expected) in [
        (None, FinishReason::Stop),
        (Some(json!(null)), FinishReason::Stop),
        (Some(json!(true)), FinishReason::Stop),
        (Some(json!(false)), FinishReason::AssistantContinuation),
    ] {
        let mut value = json!({"id":"resp_1","object":"response","status":"completed",
            "model":"gpt-test","output":[text.clone()],
            "usage":{"input_tokens":7,"output_tokens":5}});
        if let Some(signal) = signal {
            value["end_turn"] = signal;
        }
        let response = ResponsesResponse::parse(
            &value,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("valid terminal response");
        let projected = project_terminal_response(
            response,
            &replay(),
            ResponsesStorageMode::ProviderStored,
            provider_request(),
        )
        .expect("projected response");
        assert_eq!(
            projected.finish().map(FinishDetails::reason),
            Some(expected)
        );
        assert_eq!(projected.usage(), Some(Usage::new(7, 5, 0, 0, 0)));
    }

    for (output, expected) in [
        (json!([]), FinishReason::AssistantContinuation),
        (
            json!([{"id":"c1","type":"function_call","status":"completed",
            "call_id":"call1","name":"test","arguments":"{}"}]),
            FinishReason::ToolCalls,
        ),
        (
            json!([{"id":"m1","type":"message","role":"assistant", "status":"completed",
            "content":[{"type":"refusal","refusal":"cannot help"}]}]),
            FinishReason::Refusal,
        ),
    ] {
        let response = ResponsesResponse::parse(
            &json!({"id":"resp_1","object":"response","status":"completed",
                "model":"gpt-test","output":output,"end_turn":false}),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("valid terminal response");
        let reason = finish_details(&response)
            .expect("finish details")
            .expect("finish")
            .reason();
        assert_eq!(reason, expected);
    }
    assert!(matches!(
        ResponsesResponse::parse(
            &json!({"id":"resp_1","object":"response","status":"completed",
                "model":"gpt-test","output":[],"end_turn":"false"}),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        ),
        Err(ResponsesHostDecodeError::InvalidField("end_turn"))
    ));
}

#[test]
fn exact_order_usage_cache_and_reasoning_candidates_are_preserved() {
    let Ok(response) = ResponsesResponse::parse(
        &json!({
            "id":"resp_1","object":"response","status":"completed","model":"gpt-test",
            "output":[
                {"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]},
                {"id":"r1","type":"reasoning","status":"completed","summary":[],"encrypted_content":"opaque"},
                {"id":"c1","type":"custom_tool_call","status":"completed","call_id":"call1","name":"code","input":"print(1)"}
            ],
            "usage":{"input_tokens":7,"output_tokens":5,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    ) else {
        panic!("valid response test fixture");
    };
    let Ok(projected) = project_terminal_response(
        response,
        &replay(),
        ResponsesStorageMode::ProviderStored,
        provider_request(),
    ) else {
        panic!("projectable response test fixture");
    };
    assert_eq!(
        projected.finish().map(FinishDetails::reason),
        Some(FinishReason::ToolCalls)
    );
    assert_eq!(projected.usage(), Some(Usage::new(7, 5, 2, 3, 0)));
    let Some(candidate) = projected.artifact_candidate.as_ref() else {
        panic!("completed response has an artifact candidate");
    };
    assert_eq!(candidate.response_id(), "resp_1");
    assert!(!format!("{candidate:?}").contains("resp_1"));
    assert_eq!(candidate.output_count(), 3);
    assert_eq!(candidate.replay().len(), 3);
    assert_eq!(candidate.storage(), ResponsesStorageMode::ProviderStored);
    assert_eq!(
        candidate.replay()[0].purpose(),
        ResponsesReplayPurpose::OutputItem
    );
    assert_eq!(candidate.replay()[0].output_index(), 0);
    assert_eq!(
        candidate.replay()[1].purpose(),
        ResponsesReplayPurpose::OutputItem
    );
    assert_eq!(candidate.replay()[1].output_index(), 1);
    assert!(
        candidate.replay()[1]
            .envelope()
            .payload()
            .get()
            .contains("\"encrypted_content\":\"opaque\"")
    );
    assert_eq!(
        candidate.replay()[2].purpose(),
        ResponsesReplayPurpose::OutputItem
    );
    assert_eq!(candidate.replay()[2].output_index(), 2);
    assert_eq!(
        candidate
            .replay_contract()
            .for_purpose(ResponsesReplayPurpose::OutputItem)
            .map(|kind| (kind.kind().as_str(), kind.version())),
        Some(("responses.output_item", 1))
    );
}

#[test]
fn stateless_projection_emits_exact_replay_without_provider_continuation() {
    let Ok(response) = ResponsesResponse::parse(
        &json!({
            "id":"resp_stateless","object":"response","status":"completed","model":"gpt-test",
            "output":[
                {"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]}
            ]
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    ) else {
        panic!("valid stateless response test fixture");
    };
    let Ok(projected) = project_terminal_response(
        response,
        &replay(),
        ResponsesStorageMode::Stateless,
        provider_request(),
    ) else {
        panic!("stateless response must remain replayable");
    };
    let Some(candidate) = projected.artifact_candidate.as_ref() else {
        panic!("stateless response must emit a local replay candidate");
    };
    assert_eq!(candidate.storage(), ResponsesStorageMode::Stateless);
    assert_eq!(candidate.response_id(), "resp_stateless");
    assert_eq!(candidate.output_count(), 1);
    assert_eq!(candidate.replay().len(), 1);
    assert_eq!(
        candidate.replay()[0].purpose(),
        ResponsesReplayPurpose::OutputItem
    );
}

#[test]
fn grok_opaque_output_uses_its_distinct_strict_replay_kind() {
    let response = ResponsesResponse::parse(
        &json!({
            "id":"resp_grok","object":"response","status":"completed","model":"gpt-test",
            "output":[
                {"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]},
                {"id":"o1","type":"future_xai_output","status":"completed","payload":{"safe":"opaque"}}
            ]
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )
    .expect("Grok response");
    let projection = ResponsesReplayProjection::new(
        ProtocolId::new("xai.responses.grok-subscription.2026-07").expect("protocol"),
        None,
        ModelId::new("gpt-test").expect("model"),
        ReplayKindId::new("xai.output_item").expect("output kind"),
        2,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"project-host-account"])
            .expect("account binding"),
    )
    .expect("projection")
    .with_opaque_output_kind(
        ReplayKindId::new("xai.opaque_output").expect("opaque kind"),
        1,
    )
    .expect("opaque projection");
    let projected = project_terminal_response(
        response,
        &projection,
        ResponsesStorageMode::Stateless,
        provider_request(),
    )
    .expect("projected");
    let replay = projected.artifact_candidate().expect("candidate").replay();
    assert_eq!(replay[0].envelope().kind().as_str(), "xai.output_item");
    assert_eq!(replay[0].envelope().version(), 2);
    assert_eq!(replay[0].purpose(), ResponsesReplayPurpose::OutputItem);
    assert_eq!(replay[0].output_index(), 0);
    assert_eq!(replay[1].envelope().kind().as_str(), "xai.opaque_output");
    assert_eq!(replay[1].envelope().version(), 1);
    assert_eq!(
        replay[1].purpose(),
        ResponsesReplayPurpose::OpaqueOutputItem
    );
    assert_eq!(replay[1].output_index(), 1);
}

#[test]
fn failed_terminal_is_not_reclassified_as_a_successful_finish_or_artifact() {
    let Ok(response) = ResponsesResponse::parse(
        &json!({
            "id":"resp_failed","status":"failed","model":"gpt-test",
            "error":{"code":"server_error","message":"redacted native detail"}
        }),
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    ) else {
        panic!("valid failed response test fixture");
    };
    let Ok(projected) = project_terminal_response(
        response,
        &replay(),
        ResponsesStorageMode::ProviderStored,
        provider_request(),
    ) else {
        panic!("failed terminal remains an exact typed projection");
    };
    assert!(projected.finish().is_none());
    assert!(projected.artifact_candidate.is_none());
    assert!(projected.response().error().is_some());
}

#[test]
fn incomplete_finish_reason_is_exact_and_unknowns_fail_closed() {
    for (reason, expected) in [
        ("max_output_tokens", Some(FinishReason::Length)),
        ("content_filter", Some(FinishReason::Policy)),
        ("future_limit", None),
    ] {
        let response = ResponsesResponse::parse(
            &json!({
                "id":"resp_incomplete","object":"response","status":"incomplete",
                "model":"gpt-test","output":[],
                "incomplete_details":{"reason":reason}
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("typed incomplete response");
        let projected = project_terminal_response(
            response,
            &replay(),
            ResponsesStorageMode::Stateless,
            provider_request(),
        );
        match expected {
            Some(expected) => assert_eq!(
                projected
                    .expect("known reason")
                    .finish()
                    .map(FinishDetails::reason),
                Some(expected)
            ),
            None => assert!(projected.is_err(), "unknown reason was normalized"),
        }
    }
}

#[test]
fn provider_request_binding_requires_a_physical_request_identity() {
    let physical = Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"physical-request"])
        .expect("physical request digest");
    let invalid = ResponsesProviderRequestBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"not-physical"])
            .expect("other-domain digest"),
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"terminal"])
            .expect("terminal digest"),
        None,
    );
    assert!(matches!(
        invalid,
        Err(ResponsesProjectError::InvalidProviderRequestBinding)
    ));
    assert!(matches!(
        ResponsesProviderRequestBinding::new(
            physical,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"not-terminal"])
                .expect("other-domain digest"),
            None,
        ),
        Err(ResponsesProjectError::InvalidProviderRequestBinding)
    ));
    assert!(matches!(
        ResponsesProviderRequestBinding::new(physical, physical, None),
        Err(ResponsesProjectError::InvalidProviderRequestBinding)
    ));

    let bound = ResponsesProviderRequestBinding::new(
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"physical-with-request-id"],
        )
        .expect("physical request digest"),
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"terminal-with-request-id"],
        )
        .expect("terminal response digest"),
        Some(ModelProviderRequestId::new("req_sensitive_123").expect("request ID")),
    )
    .expect("provider request binding");
    assert!(!format!("{bound:?}").contains("req_sensitive_123"));
}
