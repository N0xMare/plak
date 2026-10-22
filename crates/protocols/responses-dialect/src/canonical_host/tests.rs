use std::sync::Arc;

use plakous_model::{
    AttemptId, ContentBlock, DigestDomain, FinishReason, FixedClock, ModelEvent, ModelRequest,
    ModelResponseAssembler, ProtocolId, ReplayKindId, RequestMetadata, RunId, SequenceIdGenerator,
    Timestamp, ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolOutputContract,
    TurnId, Usage,
};
use plakous_responses::{ResponsesStorageMode, ResponsesStreamLimits};
use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::{
    ResponsesFamilySseDecoder, ResponsesReplayProjection, ResponsesResponse,
    ResponsesResponseLimits, ResponsesStreamDialectData, ResponsesUsagePolicy,
    project_host::project_terminal_response,
};

const MODEL: &str = "gpt-test";

fn digest() -> Digest {
    Digest::sha256(DigestDomain::RequestPlan, 1, &[b"canonical-host-test"]).expect("test digest")
}

fn provider_request() -> crate::project_host::ResponsesProviderRequestBinding {
    crate::project_host::ResponsesProviderRequestBinding::new(
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"canonical-host-physical-request"],
        )
        .expect("physical request digest"),
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"canonical-host-terminal-response"],
        )
        .expect("terminal response digest"),
        Some(ModelProviderRequestId::new("req_canonical_1").expect("provider request ID")),
    )
    .expect("provider request binding")
}

fn ids() -> CanonicalIdSources {
    let clock = Arc::new(FixedClock::new(
        Timestamp::new(42, 0).expect("test timestamp"),
    ));
    let generator = Arc::new(
        SequenceIdGenerator::new((1_u128..=32).map(Uuid::from_u128))
            .expect("test identity sequence"),
    );
    CanonicalIdSources::new(clock, generator)
}

fn data() -> ResponsesCanonicalDialectData {
    ResponsesCanonicalDialectData::codex(
        ModelId::new(MODEL).expect("test model"),
        [ResponsesCanonicalTool::local_function(
            ToolName::new("lookup").expect("test tool"),
        )],
    )
    .expect("test dialect data")
}

fn terminal_json(status: &str) -> Value {
    let (details, item_status) = if status == "incomplete" {
        (json!({"reason":"max_output_tokens"}), "incomplete")
    } else {
        (Value::Null, "completed")
    };
    json!({
        "id":"resp_1",
        "object":"response",
        "status":status,
        "model":MODEL,
        "output":[
            {"id":"m1","type":"message","role":"assistant","status":item_status,
             "content":[{"type":"output_text","text":"answer","annotations":[
                {"type":"url_citation","start_index":0,"end_index":6,
                 "url":"https://example.test","title":"Example"}
             ]}]},
            {"id":"r1","type":"reasoning","status":item_status,
             "summary":[{"type":"summary_text","text":"checked"}],
             "encrypted_content":"opaque"},
            {"id":"f1","type":"function_call","status":"completed",
             "call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"},
            {"id":"future1","type":"future_output","status":item_status,"payload":"opaque"}
        ],
        "usage":{"input_tokens":7,"output_tokens":5,
                 "input_tokens_details":{"cached_tokens":3},
                 "output_tokens_details":{"reasoning_tokens":2}},
        "incomplete_details":details
    })
}

fn replay_projection() -> ResponsesReplayProjection {
    ResponsesReplayProjection::new(
        ProtocolId::new("openai.responses").expect("test protocol"),
        None,
        ModelId::new(MODEL).expect("test model"),
        ReplayKindId::new("responses.output_item").expect("test replay kind"),
        1,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"canonical-host-account"])
            .expect("account"),
    )
    .expect("test replay projection")
}

fn terminal(status: &str) -> ResponsesTerminalProjection {
    terminal_from_json(&terminal_json(status))
}

fn terminal_from_json(value: &Value) -> ResponsesTerminalProjection {
    let response = ResponsesResponse::parse(
        value,
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )
    .expect("test terminal response");
    project_terminal_response(
        response,
        &replay_projection(),
        ResponsesStorageMode::Stateless,
        provider_request(),
    )
    .expect("test terminal projection")
}

#[test]
fn native_compaction_output_is_rejected_without_qualified_facet() {
    let mut value = terminal_json("completed");
    value["output"] = json!([{
        "id": "compact_1",
        "type": "compaction",
        "encrypted_content": "opaque-provider-ciphertext"
    }]);
    assert!(matches!(
        project_canonical_terminal(&terminal_from_json(&value), &data(), digest(), &ids()),
        Err(ResponsesCanonicalProjectError::Unsupported(
            "Responses native compaction is not qualified"
        ))
    ));
}

#[test]
fn native_compaction_output_projects_when_facet_enrolled() {
    let mut value = terminal_json("completed");
    value["output"] = json!([{
        "id": "compact_1",
        "type": "compaction",
        "encrypted_content": "opaque-provider-ciphertext"
    }]);
    let terminal = project_canonical_terminal(
        &terminal_from_json(&value),
        &data().with_native_compaction(plakous_model::CompactionReplayBinding::new(
            ProtocolId::new("openai.responses").expect("protocol"),
            None,
            ModelId::new(MODEL).expect("model"),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"fixture-account"]).expect("account"),
        )),
        digest(),
        &ids(),
    )
    .expect("enrolled native compact projects");
    let blocks = terminal.response().output()[0].blocks();
    let ContentBlock::Compaction(block) = &blocks[0] else {
        panic!("expected compaction block, got {blocks:?}");
    };
    assert_eq!(block.summary(), "opaque-provider-ciphertext");
    assert!(
        block
            .native_item()
            .is_some_and(|item| item.contains("opaque-provider-ciphertext"))
    );
}

fn observation_projector(
    data: ResponsesCanonicalDialectData,
) -> ResponsesCanonicalObservationProjector {
    let mut projector = ResponsesCanonicalObservationProjector::new(data, digest(), None, ids());
    let started = projector.project(ResponsesStreamEventKind::ResponseCreated {
        response_id: Some("resp_stream_ptc".to_owned()),
        model: Some(MODEL.to_owned()),
    });
    assert!(!started.is_empty(), "response start must be observable");
    projector
}

fn parsed_output_item(value: Value) -> ResponsesOutputItem {
    crate::parse_output_item::<crate::ResponsesHostDecodeError>(&value).expect("test output item")
}

fn add_observed_item(
    projector: &mut ResponsesCanonicalObservationProjector,
    output_index: u32,
    value: Value,
) -> Vec<ModelEvent> {
    projector.project(ResponsesStreamEventKind::OutputItemAdded {
        output_index,
        item: parsed_output_item(value),
    })
}

#[test]
fn openrouter_terminal_requires_exact_single_provider_routing_proof() {
    let mut value = json!({
        "id":"resp_router","object":"response","status":"completed","model":MODEL,
        "output":[],"usage":{"input_tokens":1,"output_tokens":1},
        "openrouter_metadata": {
            "requested": MODEL, "strategy":"direct", "attempt":1, "is_byok":false,
            "endpoints":{"total":1,"available":[{
                "provider":"OpenAI","model":MODEL,"selected":true
            }]},
            "attempts":[{"provider":"OpenAI","model":MODEL,"status":200}],
            "pipeline":[],"params":{}
        }
    });
    let data =
        ResponsesCanonicalDialectData::openrouter(ModelId::new(MODEL).expect("test model"), [])
            .and_then(|data| data.with_router_provider("OpenAI"))
            .expect("exact router facts");
    assert!(
        project_canonical_terminal(&terminal_from_json(&value), &data, digest(), &ids()).is_ok()
    );
    value["openrouter_metadata"]["endpoints"]["available"][0]["provider"] =
        Value::String("Fallback".to_owned());
    assert!(
        project_canonical_terminal(&terminal_from_json(&value), &data, digest(), &ids()).is_err()
    );
}

#[test]
fn grok_preserves_consistent_provider_reported_actual_model() {
    let value = json!({
        "id":"resp_routed","object":"response","status":"completed",
        "model":"grok-routed-backend",
        "output":[{
            "id":"m-routed","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"answer","annotations":[]}]
        }],
        "usage":{"input_tokens":7,"output_tokens":2}
    });
    let response = ResponsesResponse::parse(
        &value,
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )
    .expect("provider-reported terminal response");
    let terminal = crate::project_host::project_terminal_response_with_model_binding(
        response,
        &replay_projection(),
        ResponsesStorageMode::Stateless,
        provider_request(),
        plakous_responses::ResponseModelBinding::ProviderReported,
    )
    .expect("provider-reported projection");
    let grok =
        ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("selected model"), [])
            .expect("Grok canonical data");
    let projected = project_canonical_terminal(&terminal, &grok, digest(), &ids())
        .expect("Grok preserves routed actual identity");
    assert_eq!(
        projected.response().actual_model().map(ModelId::as_str),
        Some("grok-routed-backend")
    );

    let codex =
        ResponsesCanonicalDialectData::codex(ModelId::new(MODEL).expect("selected model"), [])
            .expect("Codex canonical data");
    let projected = project_canonical_terminal(&terminal, &codex, digest(), &ids())
        .expect("Codex OAuth preserves provider-reported stream model");
    assert_eq!(
        projected.response().actual_model().map(ModelId::as_str),
        Some("grok-routed-backend")
    );

    let platform = ResponsesCanonicalDialectData::openai_platform(
        ModelId::new(MODEL).expect("selected model"),
        [],
    )
    .expect("Platform canonical data");
    assert!(
        project_canonical_terminal(&terminal, &platform, digest(), &ids()).is_err(),
        "OpenAI Platform keeps ExactRequest"
    );
}

fn producing_request() -> ModelRequest {
    producing_request_with_tool(false)
}

fn producing_request_with_tool(custom: bool) -> ModelRequest {
    producing_request_with_tool_and_caller(custom, plakous_model::ToolCallerDisposition::DirectOnly)
}

fn producing_request_with_tool_and_caller(
    custom: bool,
    caller: plakous_model::ToolCallerDisposition,
) -> ModelRequest {
    crate::test_support::model_request_builder(
        plakous_model::ConversationView::default(),
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(101)).expect("run"),
            TurnId::try_from(Uuid::from_u128(102)).expect("turn"),
            AttemptId::try_from(Uuid::from_u128(103)).expect("attempt"),
            Timestamp::new(42, 0).expect("request time"),
        ),
    )
    .tools(vec![
        plakous_model::ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("lookup").expect("tool"),
            "1.0.0",
            "lookup",
            if custom {
                ToolInputContract::utf8(65_536).expect("UTF-8 contract")
            } else {
                ToolInputContract::structured(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false})).expect("schema")
            },
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )
        .expect("tool definition"),
        plakous_model::ToolExecutionDisposition::Local,
        caller,
        ),
    ])
    .hosted_tools(
        (!matches!(caller, plakous_model::ToolCallerDisposition::DirectOnly))
            .then_some(plakous_model::HostedTool::ProgrammaticComposition)
            .into_iter()
            .collect(),
    )
    .build()
    .expect("producing request")
}

#[test]
fn hosted_programmatic_owner_projection_retains_mcp() {
    let programmatic = producing_request_with_tool_and_caller(
        false,
        plakous_model::ToolCallerDisposition::DirectAndProviderProgrammatic,
    );
    let local = ResponsesCanonicalDialectData::openai_platform(
        ModelId::new(MODEL).expect("model"),
        [ResponsesCanonicalTool::local_function(
            ToolName::new("lookup").expect("tool"),
        )],
    )
    .expect("local projection");
    assert!(local.validate_tool_owner_projection(&programmatic).is_ok());

    let mcp = ResponsesCanonicalDialectData::openai_platform(
        ModelId::new(MODEL).expect("model"),
        [ResponsesCanonicalTool::new(
            ToolName::new("lookup").expect("tool"),
            ResponsesCanonicalToolKind::Function,
            ResponsesCanonicalToolOwner::Mcp(McpServerId::new("lookup-mcp").expect("server")),
        )],
    )
    .expect("MCP projection");
    assert!(mcp.validate_tool_owner_projection(&programmatic).is_ok());
    assert!(
        mcp.validate_tool_owner_projection(&producing_request())
            .is_ok(),
        "ordinary direct MCP projection must remain admitted"
    );
}

#[test]
fn saturated_closed_and_no_receiver_delivery_produce_identical_terminal() {
    let projection = terminal("completed");
    let no_receiver = project_canonical_terminal(&projection, &data(), digest(), &ids())
        .expect("no-receiver terminal");

    let mut saturated = ResponsesCanonicalObservationProjector::new(data(), digest(), None, ids());
    let _ = saturated.project(ResponsesStreamEventKind::OutputTextDelta {
        output_index: 0,
        content_index: 0,
        delta: "dropped prerequisite".into(),
    });
    assert!(saturated.gapped(), "saturation-shaped gap must be sticky");
    let saturated_terminal = project_canonical_terminal(&projection, &data(), digest(), &ids())
        .expect("saturated terminal");

    let closed = ResponsesCanonicalObservationProjector::new(data(), digest(), None, ids());
    drop(closed);
    let closed_terminal = project_canonical_terminal(&projection, &data(), digest(), &ids())
        .expect("closed-receiver terminal");

    assert_eq!(no_receiver.response(), saturated_terminal.response());
    assert_eq!(no_receiver.response(), closed_terminal.response());
}

#[test]
fn terminal_preserves_text_citation_reasoning_tool_unknown_usage_and_replay() {
    let raw = terminal("completed");
    let projected =
        project_canonical_terminal(&raw, &data(), digest(), &ids()).expect("canonical terminal");
    assert_eq!(
        projected.response().finish().reason(),
        FinishReason::ToolCalls
    );
    assert_eq!(
        projected.response().usage(),
        Some(Usage::new(7, 5, 2, 3, 0))
    );
    assert_eq!(
        projected.response().provider_request_id(),
        Some("req_canonical_1")
    );
    let output = projected.canonical_output();
    assert_eq!(output.len(), 4);
    assert!(matches!(
        output[0].blocks(),
        [ContentBlock::Text(text), ContentBlock::Citation(_), ContentBlock::Opaque(_)]
            if text.phase() == Some(TextPhase::Final) && text.text() == "answer"
    ));
    assert!(matches!(
        output[1].blocks(),
        [ContentBlock::Reasoning(reasoning), ContentBlock::Opaque(opaque)]
            if reasoning.visibility() == ReasoningVisibility::Summary
                && reasoning.text() == Some("checked")
                && opaque.fallback() == &OpaqueFallback::Omit
    ));
    assert!(matches!(
        output[2].blocks(),
        [ContentBlock::ToolCall(call), ContentBlock::Opaque(_)]
            if call.owner() == &ToolExecutionOwner::Local
                && call.state() == ToolCallState::Requested
                && call.correlation().remote_call_id() == Some("call_1")
                && call.raw_arguments() == "{\"q\":1}"
    ));
    assert!(matches!(output[3].blocks(), [ContentBlock::Opaque(_)]));
    assert_eq!(
        output
            .iter()
            .map(|item| item
                .blocks()
                .iter()
                .filter(|block| matches!(block, ContentBlock::Opaque(_)))
                .count())
            .collect::<Vec<_>>(),
        vec![1, 1, 1, 1]
    );
    raw.pair_canonical(&producing_request(), projected.canonical_output().to_vec())
        .expect("exact raw/canonical pairing");
}

#[test]
fn codex_final_after_multiple_execs_omits_empty_placeholders_but_retains_replay() {
    let value: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/response_codex_final_empty_placeholders.json"
    ))
    .expect("Codex multi-exec terminal fixture");
    let raw = terminal_from_json(&value);
    let projected = project_canonical_terminal(&raw, &data(), digest(), &ids())
        .expect("empty provider placeholders are not canonical content");

    assert_eq!(projected.response().output_text(), "Done.");
    assert!(matches!(
        projected.canonical_output()[0].blocks(),
        [ContentBlock::Opaque(opaque)]
            if serde_json::from_str::<Value>(opaque.replay().payload().get())
                .is_ok_and(|native| native["summary"][0]["text"] == "")
    ));
    assert!(matches!(
        projected.canonical_output()[1].blocks(),
        [ContentBlock::Text(text), ContentBlock::Opaque(_)] if text.text() == "Done."
    ));
    raw.pair_canonical(&producing_request(), projected.canonical_output().to_vec())
        .expect("empty placeholders do not break exact native/canonical pairing");
}

#[test]
fn retained_unknown_message_content_requires_compatible_replay() {
    let terminal = json!({
        "id":"resp_unknown","object":"response","status":"completed","model":MODEL,
        "output":[{
            "id":"msg_unknown","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"unknown_part","data":"opaque-content"}]
        }]
    });
    let raw = terminal_from_json(&terminal);
    let projected = project_canonical_terminal(&raw, &data(), digest(), &ids())
        .expect("unknown message part retained");
    assert!(matches!(
        projected.canonical_output()[0].blocks(),
        [ContentBlock::Opaque(opaque)]
            if *opaque.fallback() == OpaqueFallback::CompatibleReplay
    ));
    let request = crate::test_support::model_request_builder(
        plakous_model::ConversationView::from_items(projected.canonical_output().to_vec())
            .expect("test conversation items form one valid segment"),
        producing_request().metadata(),
    )
    .build()
    .expect("historical request");
    for policy in [
        crate::ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
        crate::ResponsesRequestPolicy::supergrok_stateless(),
        crate::ResponsesRequestPolicy::openrouter_stateless(),
    ] {
        let error = crate::lower_responses_request(
            &request,
            crate::ResponsesRequestConfig::new(
                &ProtocolId::new("openai.responses").expect("protocol"),
                None,
                &ModelId::new(MODEL).expect("model"),
                policy,
                None,
            ),
        )
        .expect_err("unknown content must not be silently dropped on fresh prepare");
        assert!(matches!(
            error,
            crate::ResponsesRequestError::Content(
                crate::ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
            )
        ));
        assert_eq!(
            crate::ResponsesAttemptBuildError::Request(error).preparation_error_kind(),
            plakous_model::ModelPreparationErrorKind::ProtectedHistory,
        );
    }
}

/// A SuperGrok history holding a retained unknown output item, or an
/// assistant refusal, cannot be lowered fresh: it fails `ProtectedHistory`.
/// That is why the tool-free operation-budget wrap-up must keep the bound
/// replay of the live artifact rather than prepare fresh.
#[test]
fn grok_fresh_lowering_refuses_retained_unknown_items_and_refusals() {
    let grok = ResponsesCanonicalDialectData::grok(
        ModelId::new(MODEL).expect("selected model"),
        [ResponsesCanonicalTool::local_function(
            ToolName::new("lookup").expect("test tool"),
        )],
    )
    .expect("Grok canonical data");
    let lower = |output: Vec<Value>| {
        let value = json!({
            "id":"resp_grok","object":"response","status":"completed","model":MODEL,
            "output": output,
            "usage":{"input_tokens":7,"output_tokens":5}
        });
        let raw = terminal_from_json(&value);
        let projected = project_canonical_terminal(&raw, &grok, digest(), &ids())
            .expect("Grok projects the terminal");
        let request = crate::test_support::model_request_builder(
            plakous_model::ConversationView::from_items(projected.canonical_output().to_vec())
                .expect("test conversation items form one valid segment"),
            producing_request().metadata(),
        )
        .tool_choice(plakous_model::ToolChoice::None)
        .build()
        .expect("wrap-up request");
        crate::lower_responses_request(
            &request,
            crate::ResponsesRequestConfig::new(
                &ProtocolId::new("openai.responses").expect("protocol"),
                None,
                &ModelId::new(MODEL).expect("model"),
                crate::ResponsesRequestPolicy::supergrok_stateless(),
                None,
            ),
        )
        .map(|_| ())
    };
    let reasoning = json!({"id":"r1","type":"reasoning","status":"completed",
        "summary":[],"encrypted_content":"opaque"});
    let message = json!({"id":"m1","type":"message","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":"partial","annotations":[]}]});
    let unknown = json!({"id":"future1","type":"future_output","status":"completed",
        "payload":"opaque"});
    let refusal = json!({"id":"m2","type":"message","role":"assistant","status":"completed",
        "content":[{"type":"refusal","refusal":"no"}]});

    // Control: an ordinary Grok round lowers fresh.
    lower(vec![reasoning.clone(), message.clone()]).expect("ordinary history lowers fresh");

    let unknown_error = lower(vec![reasoning.clone(), message.clone(), unknown])
        .expect_err("retained unknown output blocks fresh lowering");
    assert_eq!(
        crate::ResponsesAttemptBuildError::Request(unknown_error).preparation_error_kind(),
        plakous_model::ModelPreparationErrorKind::ProtectedHistory,
    );
    let refusal_error = lower(vec![reasoning, message, refusal])
        .expect_err("historical refusal blocks fresh lowering");
    assert_eq!(
        crate::ResponsesAttemptBuildError::Request(refusal_error).preparation_error_kind(),
        plakous_model::ModelPreparationErrorKind::ProtectedHistory,
    );
}

#[test]
fn terminal_with_only_empty_known_content_retains_exact_replay() {
    let empty = json!({
        "id":"resp_empty","object":"response","status":"completed","model":MODEL,
        "output":[{
            "id":"msg_empty","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"","annotations":[]}]
        }]
    });
    let raw = terminal_from_json(&empty);
    let projected = project_canonical_terminal(&raw, &data(), digest(), &ids())
        .expect("completed provider placeholder remains valid native output");
    assert_eq!(projected.response().output_text(), "");
    assert!(matches!(
        projected.canonical_output()[0].blocks(),
        [ContentBlock::Opaque(opaque)]
            if *opaque.fallback() == OpaqueFallback::Omit
    ));
    let request = crate::test_support::model_request_builder(
        plakous_model::ConversationView::from_items(projected.canonical_output().to_vec())
            .expect("test conversation items form one valid segment"),
        producing_request().metadata(),
    )
    .build()
    .expect("blank historical request");
    for policy in [
        crate::ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
        crate::ResponsesRequestPolicy::supergrok_stateless(),
        crate::ResponsesRequestPolicy::openrouter_stateless(),
    ] {
        let lowered = crate::lower_responses_request(
            &request,
            crate::ResponsesRequestConfig::new(
                &ProtocolId::new("openai.responses").expect("protocol"),
                None,
                &ModelId::new(MODEL).expect("model"),
                policy,
                None,
            ),
        )
        .expect("blank historical messages omit without replay");
        assert_eq!(lowered.body()["input"], json!([]));
    }
    projected
        .response()
        .validate_for_commit()
        .expect("exact replay is a non-empty canonical assistant output");
    raw.pair_canonical(&producing_request(), projected.canonical_output().to_vec())
        .expect("replay-only output retains exact raw/canonical pairing");

    let no_output = json!({
        "id":"resp_no_output","object":"response","status":"completed","model":MODEL,
        "output":[]
    });
    let projected =
        project_canonical_terminal(&terminal_from_json(&no_output), &data(), digest(), &ids())
            .expect("empty aggregate remains representable until the commit boundary");
    assert!(projected.response().validate_for_commit().is_err());
}

#[test]
fn authoritative_events_reassemble_the_exact_terminal_response() {
    let projected = project_canonical_terminal(&terminal("completed"), &data(), digest(), &ids())
        .expect("canonical terminal");
    let expected = projected.response().clone();
    let events = projected.into_events();
    assert!(matches!(
        events.first(),
        Some(ModelEvent::ResponseStarted(_))
    ));
    assert!(matches!(
        events.last(),
        Some(ModelEvent::ResponseCompleted(_))
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
            .count(),
        1
    );

    let mut machine = ModelResponseAssembler::new(digest()).expect("test assembler");
    let mut reconstructed = None;
    for event in events {
        if let Some(response) = machine.push(event).expect("self-consistent event stream") {
            assert!(
                reconstructed.is_none(),
                "terminal response must be emitted once"
            );
            reconstructed = Some(response);
        }
    }
    machine.finish_eof().expect("complete event stream");
    assert_eq!(reconstructed.as_ref(), Some(&expected));
}

#[test]
fn incomplete_terminal_keeps_length_finish_and_never_depends_on_live_state() {
    let projected = project_canonical_terminal(&terminal("incomplete"), &data(), digest(), &ids())
        .expect("incomplete canonical terminal");
    assert_eq!(projected.response().finish().reason(), FinishReason::Length);
    assert_eq!(
        projected.response().finish().detail(),
        Some("max_output_tokens")
    );
    let events = projected.into_events();
    assert!(matches!(
        events.first(),
        Some(ModelEvent::ResponseStarted(_))
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::Usage(_)))
    );
    assert!(matches!(
        events.last(),
        Some(ModelEvent::ResponseCompleted(_))
    ));
}

#[test]
fn codex_and_grok_preserve_commentary_but_admit_optional_surfaces_differently() {
    let commentary = terminal_from_json(&json!({
        "id":"resp_commentary","object":"response","status":"completed","model":MODEL,
        "output":[{
            "id":"m-commentary","type":"message","role":"assistant","status":"completed",
            "phase":"commentary",
            "content":[{"type":"output_text","text":"working","annotations":[]}]
        }]
    }));
    let codex = ResponsesCanonicalDialectData::codex(ModelId::new(MODEL).expect("model"), [])
        .expect("Codex data");
    let grok = ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("model"), [])
        .expect("Grok data");
    for dialect in [&codex, &grok] {
        let projected = project_canonical_terminal(&commentary, dialect, digest(), &ids())
            .expect("commentary is common message semantics");
        assert!(matches!(
            projected.canonical_output()[0].blocks(),
            [ContentBlock::Text(text), ContentBlock::Opaque(_)]
                if text.text() == "working"
                    && text.phase() == Some(TextPhase::Commentary)
        ));
    }

    let citation = terminal_from_json(&json!({
        "id":"resp_citation","object":"response","status":"completed","model":MODEL,
        "output":[{
            "id":"m-citation","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"source","annotations":[{
                "type":"url_citation","start_index":0,"end_index":6,
                "url":"https://example.test/source","title":"Source"
            }]}]
        }]
    }));
    assert!(project_canonical_terminal(&citation, &codex, digest(), &ids()).is_ok());
    assert!(matches!(
        project_canonical_terminal(&citation, &grok, digest(), &ids()),
        Err(ResponsesCanonicalProjectError::Unsupported("citations"))
    ));
}

#[test]
fn codex_empty_custom_input_is_exact_and_grok_does_not_import_custom_policy() {
    let custom = terminal_from_json(&json!({
        "id":"resp_custom","object":"response","status":"completed","model":MODEL,
        "output":[{
            "id":"custom-1","type":"custom_tool_call","status":"completed",
            "call_id":"call-custom-1","name":"lookup","input":""
        }]
    }));
    let codex = ResponsesCanonicalDialectData::codex(
        ModelId::new(MODEL).expect("model"),
        [ResponsesCanonicalTool::local_custom(
            ToolName::new("lookup").expect("custom tool"),
        )],
    )
    .expect("Codex data");
    let projected = project_canonical_terminal(&custom, &codex, digest(), &ids())
        .expect("empty custom input is valid arbitrary input");
    assert!(matches!(
        projected.canonical_output()[0].blocks(),
        [ContentBlock::ToolCall(call), ContentBlock::Opaque(_)]
            if call.raw_arguments() == "\"\""
                && call.arguments() == Some(&Value::String(String::new()))
    ));
    custom
        .clone()
        .pair_canonical(
            &producing_request_with_tool(true),
            projected.canonical_output().to_vec(),
        )
        .expect("empty custom input pairs exactly with native replay");

    let grok = ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("model"), [])
        .expect("Grok data");
    assert!(matches!(
        project_canonical_terminal(&custom, &grok, digest(), &ids()),
        Err(ResponsesCanonicalProjectError::Unsupported("custom tools"))
    ));
    assert!(
        ResponsesCanonicalDialectData::grok(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_custom(
                ToolName::new("lookup").expect("custom tool"),
            )],
        )
        .is_err()
    );
}

#[test]
fn codex_lite_accepts_only_advertised_functions_namespace_calls() {
    for (kind, arguments, tool) in [
        (
            "function_call",
            json!({"arguments":"{}"}),
            ResponsesCanonicalTool::local_function(ToolName::new("lookup").expect("tool")),
        ),
        (
            "custom_tool_call",
            json!({"input":"await tools.exec_command({cmd:'pwd'})"}),
            ResponsesCanonicalTool::local_custom(ToolName::new("lookup").expect("tool")),
        ),
    ] {
        let mut item = json!({
            "id":"call_item","type":kind,"status":"completed",
            "call_id":"call_remote","name":"lookup","namespace":"functions"
        });
        item.as_object_mut()
            .expect("item object")
            .extend(arguments.as_object().expect("argument object").clone());
        let mut response = json!({
            "id":"resp_lite","object":"response","status":"completed",
            "model":MODEL,"output":[item]
        });
        let standard = ResponsesCanonicalDialectData::codex(
            ModelId::new(MODEL).expect("model"),
            [tool.clone()],
        )
        .expect("standard data");
        let lite = standard.clone().with_codex_lite_namespace();
        assert!(
            project_canonical_terminal(&terminal_from_json(&response), &standard, digest(), &ids())
                .is_err()
        );
        let projected =
            project_canonical_terminal(&terminal_from_json(&response), &lite, digest(), &ids())
                .expect("exact Lite namespace must project");
        assert!(matches!(
            projected.canonical_output()[0].blocks(),
            [ContentBlock::ToolCall(call), ContentBlock::Opaque(_)]
                if call.exposed_name().as_str() == "lookup"
        ));
        terminal_from_json(&response)
            .pair_canonical(
                &producing_request_with_tool(kind == "custom_tool_call"),
                projected.canonical_output().to_vec(),
            )
            .expect("namespaced raw item pairs with exact canonical replay");
        response["output"][0]["namespace"] = json!("foreign");
        assert!(
            project_canonical_terminal(&terminal_from_json(&response), &lite, digest(), &ids())
                .is_err()
        );
        response["output"][0]["namespace"] = json!("functions");
        response["output"][0]["name"] = json!("unadvertised");
        assert!(
            project_canonical_terminal(&terminal_from_json(&response), &lite, digest(), &ids())
                .is_err()
        );
    }
}

#[test]
fn unadvertised_function_call_projects_as_local_requested() {
    let function = terminal_from_json(&json!({
        "id":"resp_hidden","object":"response","status":"completed","model":MODEL,
        "output":[{
            "id":"function-hidden","type":"function_call","status":"completed",
            "call_id":"call-hidden","name":"process_session_poll","arguments":"{}"
        }]
    }));
    let dialect = ResponsesCanonicalDialectData::codex(
        ModelId::new(MODEL).expect("model"),
        [ResponsesCanonicalTool::local_function(
            ToolName::new("lookup").expect("advertised tool"),
        )],
    )
    .expect("Codex data");
    let projected = project_canonical_terminal(&function, &dialect, digest(), &ids())
        .expect("unadvertised names must project as local requested calls");
    assert!(matches!(
        projected.canonical_output()[0].blocks(),
        [ContentBlock::ToolCall(call), ContentBlock::Opaque(_)]
            if call.exposed_name().as_str() == "process_session_poll"
                && *call.owner() == ToolExecutionOwner::Local
                && call.state() == ToolCallState::Requested
    ));
}

#[test]
fn codex_and_grok_reject_non_object_or_non_unique_function_json() {
    for arguments in ["[]", "{\"key\":1,\"key\":2}", "not json"] {
        let function = terminal_from_json(&json!({
            "id":"resp_function","object":"response","status":"completed","model":MODEL,
            "output":[{
                "id":"function-1","type":"function_call","status":"completed",
                "call_id":"call-function-1","name":"lookup","arguments":arguments
            }]
        }));
        for dialect in [
            ResponsesCanonicalDialectData::codex(
                ModelId::new(MODEL).expect("model"),
                [ResponsesCanonicalTool::local_function(
                    ToolName::new("lookup").expect("function tool"),
                )],
            )
            .expect("Codex data"),
            ResponsesCanonicalDialectData::grok(
                ModelId::new(MODEL).expect("model"),
                [ResponsesCanonicalTool::local_function(
                    ToolName::new("lookup").expect("function tool"),
                )],
            )
            .expect("Grok data"),
        ] {
            assert!(
                project_canonical_terminal(&function, &dialect, digest(), &ids()).is_err(),
                "invalid function JSON was accepted: {arguments:?}"
            );
        }
    }
}

#[test]
fn live_projector_emits_best_effort_text_reasoning_tools_and_unknowns() {
    let fixture = include_str!("../../tests/fixtures/response_mixed.sse");
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::codex("gpt-5.4"),
        ResponsesStreamLimits::default(),
    );
    let mut family = decoder.push(fixture.as_bytes()).expect("fixture decode");
    family.extend(decoder.finish().expect("fixture finish"));
    let mut projector = ResponsesCanonicalObservationProjector::new(
        ResponsesCanonicalDialectData::codex(
            ModelId::new("gpt-5.4").expect("fixture model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("fixture tool"),
            )],
        )
        .expect("fixture dialect"),
        digest(),
        None,
        ids(),
    );
    let events = family
        .into_iter()
        .flat_map(|event| projector.project(event))
        .collect::<Vec<_>>();
    assert!(!projector.gapped(), "complete fixture must not gap");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::TextDelta(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ReasoningDelta(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ToolInputDelta(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ProviderEvent(_)))
    );
}

#[test]
fn live_projector_emits_citations_and_custom_tool_completion() {
    let fixture = include_str!("../../tests/fixtures/response_o1_surfaces.sse");
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::codex("gpt-5.4"),
        ResponsesStreamLimits::default(),
    );
    let mut family = decoder.push(fixture.as_bytes()).expect("fixture decode");
    family.extend(decoder.finish().expect("fixture finish"));
    let mut projector = ResponsesCanonicalObservationProjector::new(
        ResponsesCanonicalDialectData::codex(
            ModelId::new("gpt-5.4").expect("fixture model"),
            [
                ResponsesCanonicalTool::local_function(
                    ToolName::new("lookup").expect("fixture function tool"),
                ),
                ResponsesCanonicalTool::local_custom(
                    ToolName::new("write_code").expect("fixture custom tool"),
                ),
            ],
        )
        .expect("fixture dialect"),
        digest(),
        None,
        ids(),
    );
    let events = family
        .into_iter()
        .flat_map(|event| projector.project(event))
        .collect::<Vec<_>>();
    assert!(!projector.gapped(), "complete fixture must not gap");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::CitationAdded(_)))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::BlockStarted(started)
            if matches!(started.block(), BlockStart::Complete(ContentBlock::ToolCall(call))
                if call.exposed_name().as_str() == "write_code")
    )));
}

#[test]
fn program_caller_keeps_local_owner_and_direct_lineage_is_neighbor() {
    let value = json!({
        "id":"resp_ptc","object":"response","status":"completed","model":MODEL,
        "output":[
            {"type":"program","id":"p1","call_id":"pc1","code":"await lookup({q:1})","fingerprint":"opaque"},
            {"type":"function_call","id":"f1","call_id":"fc1","name":"lookup","arguments":"{\"q\":1}","caller":{"type":"program","caller_id":"pc1"}},
            {"type":"program_output","id":"o1","call_id":"pc1","result":"ok","status":"completed"}
        ]
    });
    let platform = ResponsesCanonicalDialectData::openai_platform(
        ModelId::new(MODEL).expect("model"),
        [ResponsesCanonicalTool::local_function(
            ToolName::new("lookup").expect("tool"),
        )],
    )
    .expect("platform data");
    let terminal = terminal_from_json(&value);
    assert!(project_canonical_terminal(&terminal, &data(), digest(), &ids()).is_err());
    let projected = project_canonical_terminal(&terminal, &platform, digest(), &ids())
        .expect("programmatic local tool projection");
    let call = projected
        .canonical_output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| {
            if let ContentBlock::ToolCall(call) = block {
                Some(call)
            } else {
                None
            }
        })
        .expect("nested tool call");
    assert_eq!(call.owner(), &ToolExecutionOwner::Local);
    assert_eq!(
        call.correlation()
            .caller_lineage()
            .provider_programmatic_execution_id(),
        Some("pc1")
    );

    for caller in [Value::Null, json!({"type":"direct"})] {
        let direct = json!({
            "id":"resp_direct","object":"response","status":"completed","model":MODEL,
            "output":[
                {"type":"function_call","id":"f1","call_id":"fc1","name":"lookup","arguments":"{}","caller":caller}
            ]
        });
        let projected =
            project_canonical_terminal(&terminal_from_json(&direct), &data(), digest(), &ids())
                .expect("direct caller projection");
        let direct_call = projected
            .canonical_output()
            .iter()
            .flat_map(ConversationItem::blocks)
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("direct tool call");
        assert!(matches!(
            direct_call.correlation().caller_lineage(),
            plakous_model::ToolCallerLineage::Direct
        ));
    }
}

#[test]
fn streaming_program_caller_requires_one_exact_preceding_program_before_tool_emission() {
    let platform = || {
        ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("tool"),
            )],
        )
        .expect("platform data")
    };

    for (case, prior, caller_id) in [
        ("missing", None, "pc_missing"),
        ("forward", None, "pc_future"),
        (
            "foreign",
            Some(json!({
                "type":"future_program","id":"foreign","call_id":"pc_foreign"
            })),
            "pc_foreign",
        ),
    ] {
        let mut projector = observation_projector(platform());
        let mut output_index = 0;
        if let Some(prior) = prior {
            let _ = add_observed_item(&mut projector, output_index, prior);
            assert!(!projector.gapped(), "{case} prior must remain admissible");
            output_index += 1;
        }
        let emitted = add_observed_item(
            &mut projector,
            output_index,
            json!({
                "type":"function_call","id":format!("f_{case}"),
                "call_id":format!("fc_{case}"),"name":"lookup","arguments":"{}",
                "caller":{"type":"program","caller_id":caller_id}
            }),
        );
        assert!(emitted.is_empty(), "{case} caller emitted a customer tool");
        assert!(projector.gapped(), "{case} caller did not fail closed");
    }

    let mut duplicate = observation_projector(platform());
    for (index, id) in [(0, "p1"), (1, "p2")] {
        let emitted = add_observed_item(
            &mut duplicate,
            index,
            json!({
                "type":"program","id":id,"call_id":"pc_duplicate",
                "code":"await lookup({})","fingerprint":"opaque"
            }),
        );
        assert!(
            index == 0 || emitted.is_empty(),
            "duplicate program emitted an observation"
        );
    }
    assert!(duplicate.gapped(), "duplicate program call_id was admitted");

    let mut ordered = observation_projector(platform());
    let _ = add_observed_item(
        &mut ordered,
        0,
        json!({
            "type":"program","id":"p1","call_id":"pc_ordered",
            "code":"await lookup({})","fingerprint":"opaque"
        }),
    );
    let emitted = add_observed_item(
        &mut ordered,
        1,
        json!({
            "type":"function_call","id":"f1","call_id":"fc_ordered",
            "name":"lookup","arguments":"{}",
            "caller":{"type":"program","caller_id":"pc_ordered"}
        }),
    );
    assert!(!ordered.gapped(), "ordered program caller must be admitted");
    assert!(emitted.iter().any(|event| matches!(
        event,
        ModelEvent::BlockStarted(started)
            if matches!(started.block(), BlockStart::ToolCall { correlation, .. }
                if correlation.caller_lineage()
                    .provider_programmatic_execution_id() == Some("pc_ordered"))
    )));
}

fn prior_program_context() -> ResponsesHostedProgramContext {
    let mut context = ResponsesHostedProgramContext::default();
    context
        .admit_replayed_output(
            &parsed_output_item(json!({
                "type":"program","id":"p_prior","call_id":"pc_prior",
                "code":"await lookup({})","fingerprint":"fp-prior"
            })),
            ResponsesCanonicalPolicy::openai_platform(),
        )
        .expect("validated prior program");
    context
}

#[test]
fn later_aggregate_and_stream_items_require_exact_prior_live_program() {
    let platform = || {
        ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("tool"),
            )],
        )
        .expect("platform data")
    };
    let later_call = json!({
        "id":"resp_later_call","object":"response","status":"completed","model":MODEL,
        "output":[{
            "type":"function_call","id":"f_later","call_id":"fc_later",
            "name":"lookup","arguments":"{}",
            "caller":{"type":"program","caller_id":"pc_prior"}
        }]
    });
    assert!(
        project_canonical_terminal(
            &terminal_from_json(&later_call),
            &platform(),
            digest(),
            &ids()
        )
        .is_err(),
        "a loose caller string must not become authority"
    );
    project_canonical_terminal_with_hosted_programs(
        &terminal_from_json(&later_call),
        &platform(),
        digest(),
        &ids(),
        prior_program_context(),
    )
    .expect("exact prior program caller");

    let later_output = json!({
        "id":"resp_later_output","object":"response","status":"completed","model":MODEL,
        "output":[{
            "type":"program_output","id":"o_later","call_id":"pc_prior",
            "result":"done","status":"completed"
        }]
    });
    project_canonical_terminal_with_hosted_programs(
        &terminal_from_json(&later_output),
        &platform(),
        digest(),
        &ids(),
        prior_program_context(),
    )
    .expect("one exact prior program terminal");

    let mut stream = ResponsesCanonicalObservationProjector::new_with_hosted_programs(
        platform(),
        digest(),
        None,
        ids(),
        prior_program_context(),
    );
    let _ = stream.project(ResponsesStreamEventKind::ResponseCreated {
        response_id: Some("resp_stream_later".to_owned()),
        model: Some(MODEL.to_owned()),
    });
    let emitted = add_observed_item(&mut stream, 0, later_call["output"][0].clone());
    assert!(!stream.gapped());
    assert!(emitted.iter().any(|event| matches!(
        event,
        ModelEvent::BlockStarted(started)
            if matches!(started.block(), BlockStart::ToolCall { correlation, .. }
                if correlation.caller_lineage()
                    .provider_programmatic_execution_id() == Some("pc_prior"))
    )));

    let mut final_stream = ResponsesCanonicalObservationProjector::new_with_hosted_programs(
        platform(),
        digest(),
        None,
        ids(),
        prior_program_context(),
    );
    let _ = final_stream.project(ResponsesStreamEventKind::ResponseCreated {
        response_id: Some("resp_stream_final".to_owned()),
        model: Some(MODEL.to_owned()),
    });
    let _ = add_observed_item(&mut final_stream, 0, later_output["output"][0].clone());
    assert!(!final_stream.gapped());
    assert_eq!(final_stream.hosted_programs.live_program_count(), 0);
}

#[test]
fn program_terminal_mutations_fail_one_to_one_correlation() {
    let platform = ResponsesCanonicalDialectData::openai_platform(
        ModelId::new(MODEL).expect("model"),
        [ResponsesCanonicalTool::local_function(
            ToolName::new("lookup").expect("tool"),
        )],
    )
    .expect("platform data");
    let mutations = [
        json!([{
            "type":"function_call","id":"f","call_id":"fc","name":"lookup",
            "arguments":"{}","caller":{"type":"program","caller_id":"unmatched"}
        }]),
        json!([
            {"type":"program_output","id":"o","call_id":"pc","result":"early","status":"completed"},
            {"type":"program","id":"p","call_id":"pc","code":"x","fingerprint":"fp"}
        ]),
        json!([
            {"type":"program","id":"p","call_id":"pc","code":"x","fingerprint":"fp"},
            {"type":"program_output","id":"o1","call_id":"pc","result":"one","status":"completed"},
            {"type":"program_output","id":"o2","call_id":"pc","result":"two","status":"completed"}
        ]),
    ];
    for (index, output) in mutations.into_iter().enumerate() {
        let value = json!({
            "id":format!("resp_mutation_{index}"),"object":"response",
            "status":"completed","model":MODEL,"output":output
        });
        assert!(
            project_canonical_terminal(&terminal_from_json(&value), &platform, digest(), &ids())
                .is_err(),
            "program mutation {index} was admitted"
        );
    }
}

#[test]
fn two_live_programs_may_complete_out_of_order_in_aggregate_and_stream() {
    let platform = ResponsesCanonicalDialectData::openai_platform(
        ModelId::new(MODEL).expect("model"),
        [ResponsesCanonicalTool::local_function(
            ToolName::new("lookup").expect("tool"),
        )],
    )
    .expect("platform data");
    let output = json!([
        {"type":"program","id":"p1","call_id":"pc1","code":"x","fingerprint":"fp1"},
        {"type":"program","id":"p2","call_id":"pc2","code":"y","fingerprint":"fp2"},
        {"type":"program_output","id":"o2","call_id":"pc2","result":"second first","status":"completed"},
        {"type":"program_output","id":"o1","call_id":"pc1","result":"first second","status":"completed"}
    ]);
    let value = json!({
        "id":"resp_out_of_order_programs","object":"response",
        "status":"completed","model":MODEL,"output":output
    });

    let projected =
        project_canonical_terminal(&terminal_from_json(&value), &platform, digest(), &ids())
            .expect("exact call IDs allow non-FIFO program completion");
    let observed_call_ids = projected
        .canonical_output()
        .iter()
        .map(|item| {
            let opaque = item
                .blocks()
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Opaque(opaque) => Some(opaque),
                    _ => None,
                })
                .expect("program item replay");
            let native: Value = serde_json::from_str(opaque.replay().payload().get())
                .expect("exact native replay JSON");
            native["call_id"]
                .as_str()
                .expect("program call_id")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(observed_call_ids, ["pc1", "pc2", "pc2", "pc1"]);

    let mut stream = observation_projector(platform);
    for (output_index, item) in output.as_array().expect("output array").iter().enumerate() {
        let _ = add_observed_item(
            &mut stream,
            u32::try_from(output_index).expect("bounded fixture index"),
            item.clone(),
        );
        assert!(
            !stream.gapped(),
            "provider-observed item {output_index} gapped"
        );
    }
    assert_eq!(stream.hosted_programs.live_program_count(), 0);
}

#[test]
fn inherited_program_terminal_and_caller_mutations_fail_aggregate_and_stream_projection() {
    let platform = ResponsesCanonicalDialectData::openai_platform(
        ModelId::new(MODEL).expect("model"),
        [ResponsesCanonicalTool::local_function(
            ToolName::new("lookup").expect("tool"),
        )],
    )
    .expect("platform data");
    let mutations = [
        json!([
            {"type":"program_output","id":"o1","call_id":"pc_prior","result":"done","status":"completed"},
            {"type":"program_output","id":"o2","call_id":"pc_prior","result":"duplicate","status":"completed"}
        ]),
        json!([
            {"type":"program_output","id":"o","call_id":"pc_prior","result":"too early","status":"completed"},
            {"type":"function_call","id":"f","call_id":"fc","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"pc_prior"}}
        ]),
        json!([
            {"type":"program_output","id":"o","call_id":"unmatched","result":"wrong","status":"completed"}
        ]),
        json!([
            {"type":"function_call","id":"f","call_id":"fc","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"unmatched"}}
        ]),
    ];
    for (index, output) in mutations.into_iter().enumerate() {
        let value = json!({
            "id":format!("resp_inherited_mutation_{index}"),
            "object":"response", "status":"completed", "model":MODEL,
            "output":output
        });
        assert!(
            project_canonical_terminal_with_hosted_programs(
                &terminal_from_json(&value),
                &platform,
                digest(),
                &ids(),
                prior_program_context(),
            )
            .is_err(),
            "inherited aggregate mutation {index} was admitted"
        );

        let mut stream = ResponsesCanonicalObservationProjector::new_with_hosted_programs(
            platform.clone(),
            digest(),
            None,
            ids(),
            prior_program_context(),
        );
        let _ = stream.project(ResponsesStreamEventKind::ResponseCreated {
            response_id: Some(format!("resp_stream_mutation_{index}")),
            model: Some(MODEL.to_owned()),
        });
        for (output_index, item) in value["output"]
            .as_array()
            .expect("mutation output")
            .iter()
            .enumerate()
        {
            let _ = add_observed_item(
                &mut stream,
                u32::try_from(output_index).expect("bounded fixture index"),
                item.clone(),
            );
        }
        assert!(
            stream.gapped(),
            "inherited stream mutation {index} was admitted"
        );
    }
}

#[test]
fn program_artifacts_and_callers_are_openai_platform_only_route_matrix() {
    let value = json!({
        "id":"resp_ptc_matrix","object":"response","status":"completed","model":MODEL,
        "output":[
            {"type":"program","id":"p1","call_id":"pc1","code":"await lookup({})","fingerprint":"opaque"},
            {"type":"function_call","id":"f1","call_id":"fc1","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"pc1"}},
            {"type":"program_output","id":"o1","call_id":"pc1","result":"ok","status":"completed"}
        ]
    });
    let terminal = terminal_from_json(&value);
    let tool = || ResponsesCanonicalTool::local_function(ToolName::new("lookup").expect("tool"));
    let routes = [
        (
            "OpenAI Platform",
            ResponsesCanonicalDialectData::openai_platform(
                ModelId::new(MODEL).expect("model"),
                [tool()],
            )
            .expect("OpenAI Platform data"),
            true,
        ),
        (
            "Codex",
            ResponsesCanonicalDialectData::codex(ModelId::new(MODEL).expect("model"), [tool()])
                .expect("Codex data"),
            false,
        ),
        (
            "Grok/SuperGrok",
            ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("model"), [tool()])
                .expect("Grok data"),
            false,
        ),
        (
            "xAI Platform",
            ResponsesCanonicalDialectData::xai_platform(ModelId::new(MODEL).expect("model"))
                .expect("xAI Platform data"),
            false,
        ),
        (
            "OpenRouter",
            ResponsesCanonicalDialectData::openrouter(
                ModelId::new(MODEL).expect("model"),
                [tool()],
            )
            .expect("OpenRouter data"),
            false,
        ),
    ];

    for (route, dialect, admitted) in routes {
        assert_eq!(
            project_canonical_terminal(&terminal, &dialect, digest(), &ids()).is_ok(),
            admitted,
            "{route} terminal program artifact policy drifted"
        );
        let mut projector = observation_projector(dialect);
        let emitted = add_observed_item(&mut projector, 0, value["output"][0].clone());
        assert_eq!(
            !projector.gapped(),
            admitted,
            "{route} streaming program artifact policy drifted"
        );
        if !admitted {
            assert!(emitted.is_empty(), "{route} promoted a program artifact");
        }
    }
}

#[test]
fn bounded_unknown_output_is_provider_neutral() {
    let value = json!({
        "id":"resp_future","object":"response","status":"completed","model":MODEL,
        "output":[{"type":"future_provider_item","id":"future1","future":{"nested":true}}]
    });
    let terminal = terminal_from_json(&value);
    let platform = ResponsesCanonicalDialectData::xai_platform(ModelId::new(MODEL).expect("model"))
        .expect("platform data");
    let projected = project_canonical_terminal(&terminal, &platform, digest(), &ids())
        .expect("bounded unknown stays opaque");
    assert!(projected.response().output().iter().all(|item| {
        item.blocks()
            .iter()
            .all(|block| !matches!(block, ContentBlock::ToolCall(_)))
    }));
    let supergrok = ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("model"), [])
        .expect("SuperGrok data");
    assert!(project_canonical_terminal(&terminal, &supergrok, digest(), &ids()).is_ok());

    let duplicate = json!({
        "id":"resp_future_duplicate","object":"response","status":"completed","model":MODEL,
        "output":[
            {"type":"future_provider_item","id":"future1","future":{"nested":true}},
            {"type":"future_provider_item","id":"future1","future":{"nested":true}}
        ]
    });
    assert!(
        ResponsesResponse::parse(
            &duplicate,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .is_err()
    );
}

/// Hosted retrieval runs only in isolated side requests: a main exchange
/// that reports a hosted search call is refused under every policy.
#[test]
fn main_exchange_refuses_hosted_search_items_under_every_policy() {
    for policy in [
        ResponsesCanonicalPolicy::minimal(),
        ResponsesCanonicalPolicy::codex(),
        ResponsesCanonicalPolicy::grok(),
    ] {
        for kind in ["web_search_call", "x_search_call"] {
            assert!(
                matches!(
                    validate_opaque_kind(kind, policy),
                    Err(ResponsesCanonicalProjectError::Unsupported(_))
                ),
                "{kind}"
            );
        }
    }
}
