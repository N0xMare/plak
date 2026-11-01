use super::*;

#[test]
fn an_in_band_rate_limit_status_with_a_quota_type_is_terminal() {
    let mut projector = projector();
    let error = projector.push(&data(&serde_json::json!({"error":{"code":429,"type":"insufficient_quota","message":"fixture quota"}}))).expect_err("quota");
    assert!(error.usage_limit());
    assert_eq!(error.http_status(), Some(429));
}

#[test]
fn openrouter_usage_chunk_is_structurally_bounded() {
    let data = r#"{"id":"gen-exact","object":"chat.completion.chunk","created":1787623695,"model":"model-literal","provider":"Nvidia","service_tier":null,"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"length","native_finish_reason":"length"}],"usage":{"prompt_tokens":18,"completion_tokens":8,"total_tokens":26,"cost":0,"is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,"audio_tokens":0,"video_tokens":0},"cost_details":{"upstream_inference_cost":0,"upstream_inference_prompt_cost":0,"upstream_inference_completions_cost":0},"completion_tokens_details":{"reasoning_tokens":8,"image_tokens":0,"audio_tokens":0}}}"#;
    let chunk: Chunk = serde_json::from_str(data).expect("OpenRouter chunk grammar");
    assert!(chunk.extensions_allowed(ChatStreamExtensions::OpenRouterV1));
    assert!(!chunk.extensions_allowed(ChatStreamExtensions::None));
}

#[test]
fn openrouter_usage_chunk_projects_cost_micros() {
    let data = r#"{"id":"gen-exact","object":"chat.completion.chunk","created":1787623695,"model":"model-literal","provider":"Nvidia","service_tier":null,"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"length","native_finish_reason":"length"}],"usage":{"prompt_tokens":18,"completion_tokens":8,"total_tokens":26,"cost":0,"is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,"audio_tokens":0,"video_tokens":0},"cost_details":{"upstream_inference_cost":0,"upstream_inference_prompt_cost":0,"upstream_inference_completions_cost":0},"completion_tokens_details":{"reasoning_tokens":8,"image_tokens":0,"audio_tokens":0}}}"#;
    let chunk: Chunk = serde_json::from_str(data).expect("OpenRouter chunk grammar");
    let usage = chunk
        .usage
        .expect("usage")
        .canonical(ChatStreamExtensions::OpenRouterV1)
        .expect("canonical usage");
    assert_eq!(usage.cost_micros(), Some(0));
}

#[test]
fn openrouter_usage_chunk_projects_fractional_cost_micros() {
    let data = r#"{"id":"gen-exact","object":"chat.completion.chunk","created":1787623695,"model":"model-literal","provider":"Nvidia","service_tier":null,"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"length","native_finish_reason":"length"}],"usage":{"prompt_tokens":18,"completion_tokens":8,"total_tokens":26,"cost":0.0001234,"is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,"audio_tokens":0,"video_tokens":0},"completion_tokens_details":{"reasoning_tokens":8,"image_tokens":0,"audio_tokens":0}}}"#;
    let chunk: Chunk = serde_json::from_str(data).expect("OpenRouter chunk grammar");
    let usage = chunk
        .usage
        .expect("usage")
        .canonical(ChatStreamExtensions::OpenRouterV1)
        .expect("canonical usage");
    assert_eq!(usage.cost_micros(), Some(123));
}

#[test]
fn openrouter_provider_chunk_captures_served_by_once() {
    let binding =
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1);
    let mut projector = projector_with_binding(binding);
    let mut role = role();
    role["provider"] = json!("Nvidia");
    push_json(&mut projector, role);
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"content": "answer"}, "finish_reason": null}])),
    );
    let _ = complete(&mut projector, "stop");
    let terminal = projector.finish().expect("terminal");
    assert_eq!(
        terminal.response().metadata().served_by(),
        Some(ServedBy::new("Nvidia").expect("provider"))
    );
}

#[test]
fn openrouter_invalid_provider_is_rejected() {
    let binding =
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1);
    let mut projector = projector_with_binding(binding);
    let mut role = role();
    role["provider"] = json!("x".repeat(65));
    assert_eq!(
        push_chunk(&mut projector, role)
            .expect_err("oversized provider")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
}

#[test]
fn openrouter_contradictory_provider_fails_stream() {
    let binding =
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1);
    let mut projector = projector_with_binding(binding);
    let mut first = role();
    first["provider"] = json!("Alpha");
    push_json(&mut projector, first);
    let mut second = role();
    second["provider"] = json!("Beta");
    assert_eq!(
        push_chunk(&mut projector, second)
            .expect_err("contradictory provider")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
}

/// Hosted-retrieval citations are a side-request grammar only: a main
/// exchange never asked for a plugin, so an annotation means the gateway
/// did something unrequested.
#[test]
fn openrouter_main_stream_refuses_url_citation_annotations() {
    let binding =
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1);
    let mut projector = projector_with_binding(binding);
    push_json(&mut projector, role());
    let cited = chunk(json!([{
        "index": 0,
        "delta": {"content": "answer", "annotations": [{
            "type": "url_citation",
            "url_citation": {"url": "https://example.com/", "title": "Example"}
        }]},
        "finish_reason": null
    }]));
    assert!(push_chunk(&mut projector, cited).is_err());
}

#[test]
fn openrouter_usage_cost_matrix() {
    let extensions = ChatStreamExtensions::OpenRouterV1;
    let canonical = |cost: Value| {
        let mut extra = BTreeMap::new();
        extra.insert("cost".into(), cost);
        UsageWire {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            prompt_tokens_details: PromptDetails::default(),
            completion_tokens_details: CompletionDetails::default(),
            extra,
        }
        .canonical(extensions)
    };
    assert_eq!(
        canonical(serde_json::json!(0)).expect("zero").cost_micros(),
        Some(0)
    );
    assert_eq!(
        canonical(serde_json::json!(0.0001234))
            .expect("fractional")
            .cost_micros(),
        Some(123)
    );
    assert_eq!(
        canonical(serde_json::json!(42))
            .expect("integer")
            .cost_micros(),
        Some(42_000_000)
    );
    assert_eq!(
        canonical(serde_json::json!(0.0001234567))
            .expect("sixth fractional digit")
            .cost_micros(),
        Some(123)
    );
    assert_eq!(
        canonical(serde_json::json!(0.00000042))
            .expect("sub-micro rounds to zero")
            .cost_micros(),
        Some(0)
    );
    assert_eq!(
        canonical(serde_json::json!(1e-7))
            .expect("one tenth micro")
            .cost_micros(),
        Some(0)
    );
    assert_eq!(
        canonical(serde_json::json!(0.0000015))
            .expect("one and a half micros")
            .cost_micros(),
        Some(2)
    );
    assert_eq!(
        canonical(serde_json::json!(1e-6))
            .expect("exactly one micro")
            .cost_micros(),
        Some(1)
    );
    assert_eq!(
        canonical(serde_json::json!(0.000123456789))
            .expect("long fractional")
            .cost_micros(),
        Some(123)
    );
    let cost_number =
        |text: &str| -> Value { serde_json::from_str(text).expect("decimal literal") };
    assert_eq!(
        canonical(cost_number("0.0001235678"))
            .expect("round seventh digit")
            .cost_micros(),
        Some(124)
    );
    assert_eq!(
        canonical(serde_json::json!(null))
            .expect("null")
            .cost_micros(),
        None
    );
    assert_eq!(
        canonical(serde_json::json!(-1))
            .expect_err("negative")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
    assert_eq!(
        canonical(serde_json::json!("free"))
            .expect_err("non-number")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
    assert_eq!(
        canonical(Value::Number(Number::from(18_446_744_073_795_u64)))
            .expect_err("overflow")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
}

#[test]
fn openrouter_usage_chunk_projects_cache_write_tokens() {
    let data = r#"{"id":"gen-exact","object":"chat.completion.chunk","created":1787623695,"model":"model-literal","provider":"Nvidia","service_tier":null,"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"length","native_finish_reason":"length"}],"usage":{"prompt_tokens":18,"completion_tokens":8,"total_tokens":26,"cost":0,"is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":5,"audio_tokens":0,"video_tokens":0},"cost_details":{"upstream_inference_cost":0,"upstream_inference_prompt_cost":0,"upstream_inference_completions_cost":0},"completion_tokens_details":{"reasoning_tokens":8,"image_tokens":0,"audio_tokens":0}}}"#;
    let chunk: Chunk = serde_json::from_str(data).expect("OpenRouter chunk grammar");
    let usage = chunk
        .usage
        .expect("usage")
        .canonical(ChatStreamExtensions::OpenRouterV1)
        .expect("canonical usage");
    assert_eq!(usage.cache_write_tokens(), 5);
}

#[test]
fn openrouter_router_metadata_is_observational_and_additive() {
    let value = json!({
        "id": "gen-exact",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "vendor/model",
        "provider": "Provider",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "openrouter_metadata": {
            "requested": "vendor/model",
            "strategy": "fallback",
            "attempt": 2,
            "pipeline": [{
                "type": "future_gateway_stage",
                "data": {"opaque_future_member": [1, true, null]}
            }],
            "future_gateway_member": {"opaque": true}
        }
    });
    let chunk: Chunk = serde_json::from_value(value).expect("gateway metadata JSON");
    assert!(chunk.extensions_allowed(ChatStreamExtensions::OpenRouterV1));
    assert!(!chunk.extensions_allowed(ChatStreamExtensions::None));
}

#[test]
fn openrouter_system_fingerprint_is_observational() {
    let value = json!({
        "id": "gen-exact",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "vendor/model",
        "provider": "Provider",
        "system_fingerprint": "fp-observed",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let chunk: Chunk = serde_json::from_value(value).expect("fingerprint JSON");
    assert!(chunk.extensions_allowed(ChatStreamExtensions::OpenRouterV1));
    assert!(!chunk.extensions_allowed(ChatStreamExtensions::None));
}
use plakous_model::{
    DigestDomain, EffectiveProviderConfigurationId, FixedClock, ModelResponseAssembler,
    SelectedProviderConfigurationId, SequenceIdGenerator, Timestamp,
};
use serde_json::{Number, Value, json};
use uuid::Uuid;

fn binding(limits: ChatStreamLimits) -> PreparedChatStreamBinding {
    binding_with_policy(
        limits,
        ChatReasoningReplay::ExactDetails,
        ReasoningAliasPolicy::Reasoning,
    )
}

fn binding_with_policy(
    limits: ChatStreamLimits,
    replay: ChatReasoningReplay,
    alias: ReasoningAliasPolicy,
) -> PreparedChatStreamBinding {
    let selected: SelectedProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::SelectedProviderConfiguration,
                5,
                &[b"chat-stream-selected"],
            )
            .expect("selected digest"),
        )
        .expect("selected value"),
    )
    .expect("selected identity");
    let effective: EffectiveProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"chat-stream-effective"],
            )
            .expect("effective digest"),
        )
        .expect("effective value"),
    )
    .expect("effective identity");
    let protocol = ProtocolId::new("chat-completions.v1").expect("protocol");
    let model = ModelId::new("model-literal").expect("model");
    let replay_target = ChatReplayTargetV1::new(
        protocol.clone(),
        model.clone(),
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"]).expect("account"),
        selected,
        effective,
    );
    PreparedChatStreamBinding::new(
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"chat-stream-test"]).expect("digest"),
        protocol,
        model,
        replay_target,
        ReasoningPolicy::Disabled,
        replay,
        alias,
        limits,
    )
}

fn projector() -> ChatStreamProjector {
    projector_with_limits(ChatStreamLimits::default())
}

fn projector_with_limits(limits: ChatStreamLimits) -> ChatStreamProjector {
    projector_with_binding(binding(limits))
}

fn projector_with_binding(binding: PreparedChatStreamBinding) -> ChatStreamProjector {
    ChatStreamProjector::new(
        binding,
        Some(ModelProviderRequestId::new("request-head-1").expect("request id")),
        Arc::new(FixedClock::new(
            Timestamp::new(1_700_000_000, 0).expect("time"),
        )),
        Arc::new(SequenceIdGenerator::new((1_u128..=32).map(Uuid::from_u128)).expect("ids")),
    )
}

fn chunk(choices: Value) -> Value {
    json!({
        "id": "response-1",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": "model-literal",
        "choices": choices
    })
}

fn data(value: &Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

fn push_json(projector: &mut ChatStreamProjector, value: Value) -> Vec<ModelEvent> {
    projector.push(&data(&value)).expect("valid chunk")
}

fn push_chunk(
    projector: &mut ChatStreamProjector,
    value: Value,
) -> Result<Vec<ModelEvent>, ChatStreamError> {
    projector.push(&data(&value))
}

fn role() -> Value {
    chunk(json!([{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]))
}

fn finish(reason: &str) -> Value {
    chunk(json!([{"index": 0, "delta": {}, "finish_reason": reason}]))
}

fn usage() -> Value {
    json!({
        "id": "response-1",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": "model-literal",
        "choices": [],
        "usage": {
            "prompt_tokens": 11,
            "completion_tokens": 7,
            "total_tokens": 18,
            "prompt_tokens_details": {"cached_tokens": 3},
            "completion_tokens_details": {"reasoning_tokens": 4}
        }
    })
}

fn complete(projector: &mut ChatStreamProjector, reason: &str) -> Vec<ModelEvent> {
    let mut events = push_json(projector, finish(reason));
    events.extend(push_json(projector, usage()));
    events.extend(projector.push(b"data: [DONE]\n\n").expect("done"));
    events
}

fn detail_chunk(details: &str) -> Vec<u8> {
    format!(
        "data: {{\"id\":\"response-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"model-literal\",\"choices\":[{{\"index\":0,\"delta\":{{\"reasoning_details\":[{details}]}},\"finish_reason\":null}}]}}\n\n"
    )
    .into_bytes()
}

#[test]
fn projects_fragmented_text_reasoning_usage_and_terminal_response() {
    let mut projector = projector();
    let role_bytes = data(&role());
    let mut events = Vec::new();
    for byte in role_bytes {
        events.extend(projector.push(&[byte]).expect("fragment"));
    }
    events.extend(push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"reasoning": "why "}, "finish_reason": null}])),
    ));
    events.extend(push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"reasoning": "so", "content": "answer"}, "finish_reason": null}])),
    ));
    events.extend(complete(&mut projector, "stop"));

    assert!(matches!(
        events.first(),
        Some(ModelEvent::ResponseStarted(_))
    ));
    assert!(
        events.iter().any(
            |event| matches!(event, ModelEvent::TextDelta(delta) if delta.bytes() == b"answer")
        )
    );
    assert!(events.iter().any(
        |event| matches!(event, ModelEvent::ReasoningDelta(delta) if delta.bytes() == b"why ")
    ));
    assert!(events.iter().any(|event| matches!(event, ModelEvent::Usage(update) if update.usage() == Usage::new(11, 7, 4, 3, 0))));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
    );

    let terminal = projector.finish().expect("terminal");
    assert_eq!(terminal.finish().wire(), "stop");
    assert_eq!(terminal.finish().canonical(), FinishReason::Stop);
    assert_eq!(
        projector
            .finish()
            .expect_err("terminal moves out once")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
    let response = terminal.response();
    assert_eq!(response.response_id(), Some("response-1"));
    assert_eq!(response.provider_request_id(), Some("request-head-1"));
    assert_eq!(
        response.actual_model().map(ModelId::as_str),
        Some("model-literal")
    );
    assert_eq!(response.usage(), Some(Usage::new(11, 7, 4, 3, 0)));
    assert!(response.family_artifact_candidate().is_none());
    let blocks = response.output()[0].blocks();
    assert!(
        matches!(&blocks[0], ContentBlock::Reasoning(block) if block.visibility() == ReasoningVisibility::Visible && block.text() == Some("why so"))
    );
    assert!(matches!(&blocks[1], ContentBlock::Text(block) if block.text() == "answer"));
}

#[test]
fn enforces_fixed_and_response_stable_reasoning_aliases() {
    let limits = ChatStreamLimits::default();
    let mut content = projector_with_binding(binding_with_policy(
        limits,
        ChatReasoningReplay::VisibleOnly,
        ReasoningAliasPolicy::ReasoningContent,
    ));
    push_json(&mut content, role());
    let bytes = data(&chunk(json!([{
        "index": 0,
        "delta": {"reasoning_content": "mémoire"},
        "finish_reason": null
    }])));
    for byte in bytes {
        content.push(&[byte]).expect("fragmented UTF-8 alias");
    }
    complete(&mut content, "stop");
    assert!(matches!(
        &content.finish().expect("content alias").response().output()[0].blocks()[0],
        ContentBlock::Reasoning(block) if block.text() == Some("mémoire")
    ));

    for (alias, delta) in [
        (
            ReasoningAliasPolicy::Reasoning,
            json!({"reasoning_content": "wrong"}),
        ),
        (
            ReasoningAliasPolicy::ReasoningContent,
            json!({"reasoning": "wrong"}),
        ),
        (
            ReasoningAliasPolicy::EitherBindFirst,
            json!({"reasoning": "a", "reasoning_content": "b"}),
        ),
    ] {
        let mut rejected = projector_with_binding(binding_with_policy(
            limits,
            ChatReasoningReplay::VisibleOnly,
            alias,
        ));
        push_json(&mut rejected, role());
        let error = rejected
            .push(&data(&chunk(json!([{
                "index": 0,
                "delta": delta,
                "finish_reason": null
            }]))))
            .expect_err("alias rejected");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    }

    let mut switched = projector_with_binding(binding_with_policy(
        limits,
        ChatReasoningReplay::VisibleOnly,
        ReasoningAliasPolicy::EitherBindFirst,
    ));
    push_json(&mut switched, role());
    push_json(
        &mut switched,
        chunk(json!([{
            "index": 0,
            "delta": {"reasoning": "bound"},
            "finish_reason": null
        }])),
    );
    let error = switched
        .push(&data(&chunk(json!([{
            "index": 0,
            "delta": {"reasoning_content": "switch"},
            "finish_reason": null
        }]))))
        .expect_err("response alias switch rejected");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end fixture verifies exact replay, terminal ordering, and assembly"
)]
fn projects_ordered_exact_reasoning_details_through_the_assembler() {
    let binding = binding_with_policy(
        ChatStreamLimits::default(),
        ChatReasoningReplay::ExactDetails,
        ReasoningAliasPolicy::EitherBindFirst,
    );
    let plan = binding.plan();
    let mut projector = projector_with_binding(binding);
    let mut events = push_json(&mut projector, role());
    events.extend(push_json(
        &mut projector,
        chunk(json!([{
            "index": 0,
            "delta": {"reasoning_content": "visible"},
            "finish_reason": null
        }])),
    ));
    let first = r#"{ "z" : "\u0061", "signature" : "sig\/one" }"#;
    let second = r#"{"encrypted":"A\\u003dB","index":7}"#;
    events.extend(
        projector
            .push(&detail_chunk(&format!("{first},{second}")))
            .expect("two exact details"),
    );
    events.extend(
        projector
            .push(&detail_chunk(r#"{"third" : [3, 2, 1]}"#))
            .expect("detail in later chunk"),
    );
    events.extend(push_json(
        &mut projector,
        chunk(json!([{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call-a",
                "type": "function",
                "function": {"name": "lookup", "arguments": "{}"}
            }]},
            "finish_reason": null
        }])),
    ));
    events.extend(complete(&mut projector, "tool_calls"));

    let detail_starts: Vec<_> = events
        .iter()
        .filter_map(|event| {
            let ModelEvent::BlockStarted(started) = event else {
                return None;
            };
            let BlockStart::Complete(ContentBlock::Reasoning(block)) = started.block() else {
                return None;
            };
            (block.visibility() == ReasoningVisibility::OpaqueReplay)
                .then_some((started.location(), block))
        })
        .collect();
    assert_eq!(detail_starts.len(), 3);
    let expected = [first, second, r#"{"third" : [3, 2, 1]}"#];
    for (ordinal, ((location, block), exact)) in detail_starts.iter().zip(expected).enumerate() {
        assert_eq!(
            location.block_index(),
            u32::try_from(ordinal + 1).expect("index")
        );
        let bound: ChatReasoningDetailReplayV1 =
            serde_json::from_str(block.replay().expect("detail replay").payload().get())
                .expect("bound payload");
        assert_eq!(
            bound.source_ordinal(),
            u32::try_from(ordinal).expect("ordinal")
        );
        assert_eq!(bound.exact_json(), exact);
        assert_eq!(bound.alias_policy(), ReasoningAliasPolicy::EitherBindFirst);
        assert_eq!(bound.bound_alias(), ReasoningAliasPolicy::ReasoningContent);
    }
    let detail_locations: BTreeSet<_> = detail_starts
        .iter()
        .map(|(location, _)| *location)
        .collect();
    assert!(!events.iter().any(|event| {
        matches!(event, ModelEvent::BlockCompleted(completed) if detail_locations.contains(&completed.location()))
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
    );
    assert!(events.iter().any(|event| {
        matches!(event, ModelEvent::BlockStarted(started) if matches!(started.block(), BlockStart::ToolCall { .. }))
    }));

    let terminal = projector.finish().expect("terminal");
    assert!(terminal.response().family_artifact_candidate().is_none());
    let terminal_blocks = terminal.response().output()[0].blocks();
    assert_eq!(terminal_blocks.len(), 5);
    for (location, event_block) in detail_starts {
        assert_eq!(
            &terminal_blocks[location.block_index() as usize],
            &ContentBlock::Reasoning(event_block.clone())
        );
    }
    let checkpoint = serde_json::to_vec(terminal.response()).expect("checkpoint JSON");
    let restored: ModelResponse =
        serde_json::from_slice(&checkpoint).expect("checkpoint round trip");
    for (block, exact) in restored.output()[0].blocks()[1..4].iter().zip(expected) {
        let ContentBlock::Reasoning(reasoning) = block else {
            panic!("restored detail must remain reasoning");
        };
        let bound: ChatReasoningDetailReplayV1 =
            serde_json::from_str(reasoning.replay().expect("restored replay").payload().get())
                .expect("restored bound payload");
        assert_eq!(bound.exact_json(), exact);
    }
    let mut assembler = ModelResponseAssembler::new(plan).expect("assembler");
    let mut terminal_response = None;
    events.push(ModelEvent::ResponseCompleted(terminal.response().clone()));
    for event in events {
        if let Some(response) = assembler.push(event).expect("valid lifecycle") {
            terminal_response = Some(response);
        }
    }
    assert_eq!(terminal_response.as_ref(), Some(terminal.response()));
}

#[test]
fn independently_bounds_reasoning_detail_count_and_bytes() {
    let nz = |value| NonZeroUsize::new(value).expect("nonzero");
    for (limits, details) in [
        (
            ChatStreamLimits::default().with_reasoning_detail_limits(nz(1), nz(1024)),
            r#"{"a":1},{"b":2}"#,
        ),
        (
            ChatStreamLimits::default().with_reasoning_detail_limits(nz(8), nz(6)),
            r#"{"long":true}"#,
        ),
    ] {
        let mut projector = projector_with_binding(binding_with_policy(
            limits,
            ChatReasoningReplay::ExactDetails,
            ReasoningAliasPolicy::Reasoning,
        ));
        push_json(&mut projector, role());
        let error = projector
            .push(&detail_chunk(details))
            .expect_err("independent detail limit");
        assert_eq!(error.kind(), ChatStreamErrorKind::ResourceLimit);
    }

    let mut excluded = projector_with_binding(binding_with_policy(
        ChatStreamLimits::default(),
        ChatReasoningReplay::VisibleOnly,
        ReasoningAliasPolicy::Reasoning,
    ));
    push_json(&mut excluded, role());
    assert_eq!(
        excluded
            .push(&detail_chunk(r#"{"a":1}"#))
            .expect_err("details excluded")
            .kind(),
        ChatStreamErrorKind::UnsupportedSemantic
    );
}

#[test]
fn assembles_dense_local_function_calls_with_exact_raw_arguments() {
    let mut projector = projector();
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
        {"index": 0, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":"}},
        {"index": 1, "id": "call-b", "type": "function", "function": {"name": "write", "arguments": "{\"n\":"}}
    ]}, "finish_reason": null}])),
    );
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
        {"index": 0, "function": {"arguments": "\"x\"}"}},
        {"index": 1, "function": {"arguments": "2}"}}
    ]}, "finish_reason": null}])),
    );
    let events = complete(&mut projector, "tool_calls");

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelEvent::ToolInputDelta(_)))
            .count(),
        2
    );
    let terminal = projector.finish().expect("terminal");
    let blocks = terminal.response().output()[0].blocks();
    assert_eq!(blocks.len(), 2);
    assert!(
        matches!(&blocks[0], ContentBlock::ToolCall(call) if call.exposed_name().as_str() == "lookup" && call.raw_arguments() == "{\"q\":\"x\"}" && call.correlation().remote_call_id() == Some("call-a") && call.owner() == &ToolExecutionOwner::Local)
    );
    assert!(
        matches!(&blocks[1], ContentBlock::ToolCall(call) if call.exposed_name().as_str() == "write" && call.raw_arguments() == "{\"n\":2}")
    );
}

/// A `length` stop can cut a function call anywhere in its arguments. Neither a
/// complete nor a cut call becomes canonical output: the terminal is an empty
/// `Length`, which the driver classifies as OutputLimit.
#[test]
fn length_with_tool_calls_revokes_complete_and_truncated_arguments() {
    for arguments in ["{\"q\":\"x\"}", "{\"q\":\"cut mid-str"] {
        let binding = binding(ChatStreamLimits::default());
        let plan = binding.plan();
        let mut projector = projector_with_binding(binding);
        let mut events = push_json(&mut projector, role());
        events.extend(push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"reasoning": "plan", "content": "Looking it up."}, "finish_reason": null}])),
        ));
        events.extend(push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": arguments}}
            ]}, "finish_reason": null}])),
        ));
        events.extend(complete(&mut projector, "length"));
        assert!(!events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolInputDelta(_)
        ) || matches!(
            event,
            ModelEvent::BlockStarted(started) if matches!(started.block(), BlockStart::ToolCall { .. })
        )));

        let terminal = projector.finish().expect("length terminal");
        assert_eq!(terminal.finish().wire(), "length");
        assert_eq!(terminal.finish().canonical(), FinishReason::Length);
        let response = terminal.response();
        assert_eq!(response.output(), []);
        assert_eq!(response.usage(), Some(Usage::new(11, 7, 4, 3, 0)));
        assert_eq!(response.response_id(), Some("response-1"));
        assert!(response.validate_for_commit().is_ok());
        let mut assembler = ModelResponseAssembler::new(plan).expect("assembler");
        let mut terminal_response = None;
        events.push(ModelEvent::ResponseCompleted(response.clone()));
        for event in events {
            if let Some(response) = assembler.push(event).expect("revoked lifecycle") {
                terminal_response = Some(response);
            }
        }
        assert_eq!(terminal_response.as_ref(), Some(response));
    }
}

#[test]
fn non_length_finishes_keep_strict_tool_call_agreement() {
    for reason in ["stop", "content_filter"] {
        let mut projector = projector();
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
            ]}, "finish_reason": null}])),
        );
        push_json(&mut projector, finish(reason));
        push_json(&mut projector, usage());
        let error = projector
            .push(b"data: [DONE]\n\n")
            .expect_err("tool calls under a non-tool finish");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(
            error
                .message()
                .contains("finish reason disagrees with tool calls")
        );
    }
    let mut projector = projector();
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"content": "no call"}, "finish_reason": null}])),
    );
    push_json(&mut projector, finish("tool_calls"));
    push_json(&mut projector, usage());
    assert!(
        projector
            .push(b"data: [DONE]\n\n")
            .expect_err("tool_calls without a call")
            .message()
            .contains("finish reason disagrees with tool calls")
    );
}

#[test]
fn rejects_choice_identity_object_and_unknown_wire_fields() {
    for (value, kind) in [
        (chunk(json!([])), ChatStreamErrorKind::InvalidResponse),
        (
            chunk(json!([
                {"index": 0, "delta": {}, "finish_reason": null},
                {"index": 1, "delta": {}, "finish_reason": null}
            ])),
            ChatStreamErrorKind::InvalidResponse,
        ),
        (
            json!({"id":"response-1","object":"other","created":1700000000,"model":"model-literal","choices":[{"index":0,"delta":{},"finish_reason":null}]}),
            ChatStreamErrorKind::InvalidResponse,
        ),
        (
            json!({"id":"response-1","object":"chat.completion.chunk","created":1700000000,"model":"other-model","choices":[{"index":0,"delta":{},"finish_reason":null}]}),
            ChatStreamErrorKind::InvalidResponse,
        ),
        (
            json!({"id":"response-1","object":"chat.completion.chunk","created":1700000000,"model":"model-literal","choices":[{"index":0,"delta":{},"finish_reason":null}],"extra":true}),
            ChatStreamErrorKind::InvalidResponse,
        ),
    ] {
        let error = projector().push(&data(&value)).expect_err("rejected");
        assert_eq!(error.kind(), kind);
    }

    let mut changed_created = projector();
    push_json(&mut changed_created, role());
    let mut changed = chunk(json!([{"index":0,"delta":{},"finish_reason":null}]));
    changed["created"] = json!(1_700_000_001_u64);
    assert_eq!(
        changed_created
            .push(&data(&changed))
            .expect_err("created changed")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
}

#[test]
fn openrouter_rejects_unknown_semantics_outside_reviewed_metadata() {
    let mut projector = ChatStreamProjector::new(
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1),
        Some(ModelProviderRequestId::new("request-1").expect("request id")),
        Arc::new(FixedClock::new(
            Timestamp::new(1_700_000_000, 0).expect("timestamp"),
        )),
        Arc::new(
            SequenceIdGenerator::new((1_u128..=8).map(Uuid::from_u128)).expect("sequence ids"),
        ),
    );
    let value = json!({
        "id": "response-1",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": "model-literal",
        "provider": "reviewed-metadata",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "future_hosted_action": {"id": "effect-1"}
            },
            "finish_reason": null,
            "native_finish_reason": null
        }]
    });
    assert_eq!(
        projector
            .push(&data(&value))
            .expect_err("unknown semantic")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );

    let mut sibling = projector_with_binding(
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1),
    );
    let mut value = role();
    value["future_gateway_action"] = json!({"id": "effect-2"});
    let error = sibling
        .push(&data(&value))
        .expect_err("unknown top-level semantic");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    assert!(error.message().contains("chunk.future_gateway_action"));
}

#[test]
fn openrouter_metadata_is_ignored_through_the_terminal_projector_lifecycle() {
    let openrouter_binding =
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1);
    let mut openrouter_projector = projector_with_binding(openrouter_binding);
    push_json(&mut openrouter_projector, role());
    push_json(
        &mut openrouter_projector,
        chunk(json!([{
            "index": 0,
            "delta": {"content": "answer"},
            "finish_reason": null
        }])),
    );
    push_json(&mut openrouter_projector, finish("stop"));
    let mut final_usage = usage();
    final_usage["openrouter_metadata"] = json!({
        "attempt": 2,
        "pipeline": [{
            "type": "future_gateway_stage",
            "data": {"opaque_future_member": [1, true, null]}
        }],
        "future_gateway_member": {"opaque": true}
    });
    push_json(&mut openrouter_projector, final_usage.clone());
    openrouter_projector
        .push(b"data: [DONE]\n\n")
        .expect("done");
    let terminal = openrouter_projector.finish().expect("terminal");
    assert_eq!(terminal.finish().canonical(), FinishReason::Stop);
    assert_eq!(
        terminal.response().usage(),
        Some(Usage::new(11, 7, 4, 3, 0))
    );
    assert!(matches!(
        &terminal.response().output()[0].blocks()[0],
        ContentBlock::Text(block) if block.text() == "answer"
    ));

    let mut generic = projector();
    let error = generic
        .push(&data(&final_usage))
        .expect_err("OpenRouter metadata outside its prepared dialect");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    assert!(error.message().contains("chunk.openrouter_metadata"));
}

#[test]
fn any_projection_failure_terminally_poisons_the_projector() {
    let mut projector = projector();
    assert_eq!(
        projector
            .push(b"data: {bad}\n\n")
            .expect_err("malformed JSON")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
    assert_eq!(
        projector
            .push(&data(&role()))
            .expect_err("poisoned projector")
            .message(),
        "Chat projector already failed"
    );
    assert_eq!(
        projector.finish().expect_err("poisoned finish").message(),
        "Chat projector already failed"
    );
}

#[test]
fn rejects_incomplete_and_post_terminal_lifecycles() {
    let mut no_done = projector();
    push_json(&mut no_done, role());
    push_json(
        &mut no_done,
        chunk(json!([{"index":0,"delta":{"content":"x"},"finish_reason":null}])),
    );
    assert_eq!(
        no_done.finish().expect_err("missing done").kind(),
        ChatStreamErrorKind::Incomplete
    );

    let mut no_usage = projector();
    push_json(&mut no_usage, role());
    push_json(
        &mut no_usage,
        chunk(json!([{"index":0,"delta":{"content":"x"},"finish_reason":null}])),
    );
    push_json(&mut no_usage, finish("stop"));
    assert_eq!(
        no_usage
            .push(b"data: [DONE]\n\n")
            .expect_err("missing usage")
            .kind(),
        ChatStreamErrorKind::Incomplete
    );

    let mut post = projector();
    push_json(&mut post, role());
    push_json(
        &mut post,
        chunk(json!([{"index":0,"delta":{"content":"x"},"finish_reason":null}])),
    );
    complete(&mut post, "stop");
    assert_eq!(
        post.push(b"data: {}\n\n")
            .expect_err("post terminal")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
}

#[test]
fn rejects_conflicting_and_malformed_tool_calls() {
    let cases = [
        json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"hosted","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]),
    ];
    for choices in cases {
        let mut projector = projector();
        push_json(&mut projector, role());
        assert!(projector.push(&data(&chunk(choices))).is_err());
    }

    let mut unbound = projector();
    push_json(&mut unbound, role());
    push_json(
        &mut unbound,
        chunk(
            json!([{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]},"finish_reason":null}]),
        ),
    );
    push_json(&mut unbound, finish("tool_calls"));
    push_json(&mut unbound, usage());
    let error = unbound.push(b"data: [DONE]\n\n").expect_err("unbound slot");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    assert!(
        error
            .message()
            .contains("tool-call slot never bound id and name")
    );

    let mut malformed = projector();
    push_json(&mut malformed, role());
    push_json(
        &mut malformed,
        chunk(
            json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"lookup","arguments":"{"}}]},"finish_reason":null}]),
        ),
    );
    push_json(&mut malformed, finish("tool_calls"));
    push_json(&mut malformed, usage());
    assert_eq!(
        malformed
            .push(b"data: [DONE]\n\n")
            .expect_err("malformed arguments")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );

    for raw in ["{\"x\":1,\"x\":2}", "[]"] {
        let mut ambiguous = projector();
        push_json(&mut ambiguous, role());
        push_json(
            &mut ambiguous,
            chunk(
                json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"lookup","arguments":raw}}]},"finish_reason":null}]),
            ),
        );
        push_json(&mut ambiguous, finish("tool_calls"));
        push_json(&mut ambiguous, usage());
        assert_eq!(
            ambiguous
                .push(b"data: [DONE]\n\n")
                .expect_err("ambiguous arguments")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
    }
}

/// The status an in-band error names, wherever the gateway put it.
///
/// The live shape is the last one: an edge that could not reach its
/// upstream relays that upstream's body, and the body carries the status
/// only in prose. The field diagnostic was "Chat provider returned an
/// in-band error: error code: 520", so the status was there and nothing
/// could read it.
#[test]
fn an_in_band_error_yields_the_status_the_gateway_named() {
    use super::in_band_error_status;

    for (payload, expected) in [
        (json!({"code": 520}), Some(520)),
        (json!({"code": "429"}), Some(429)),
        (
            json!({"code": 502, "message": "upstream unavailable"}),
            Some(502),
        ),
        (json!({"message": "error code: 520"}), Some(520)),
        (
            json!({"message": "Error Code: 429 from the edge"}),
            Some(429),
        ),
        (json!("error code: 524"), Some(524)),
        // Nothing machine-readable and no anchored spelling: the status
        // stays unknown and the conservative classification stands.
        (json!({"message": "Provider returned error"}), None),
        (json!({"code": "failure"}), None),
        (json!({"code": 99}), None),
        (json!({"message": "error code: 9999"}), None),
        (json!({}), None),
    ] {
        assert_eq!(in_band_error_status(&payload), expected, "{payload}");
    }
}

/// A well-formed in-band error object is the provider's own error, read
/// like the same status before acceptance: `HTTP <status> (<class>)`, the
/// upstream the gateway named, then whatever the message adds.
#[test]
fn an_in_band_error_reads_as_the_status_class_and_upstream_it_named() {
    let mut cases = vec![
        // The live shape: the edge's own spelling, a numeric code and the
        // upstream in the gateway's error metadata.
        (
            json!({"code": 504, "message": "error code: 504", "metadata": {"provider_name": "Upstream"}}),
            Some(504),
            "HTTP 504 (timeout) from Upstream",
        ),
        (json!({"code": 504}), Some(504), "HTTP 504 (timeout)"),
        (
            json!({"code": "504", "message": "upstream timed out"}),
            Some(504),
            "HTTP 504 (timeout): upstream timed out",
        ),
        (
            json!({"code": 504, "type": "timeout"}),
            Some(504),
            "HTTP 504 (timeout): type=timeout",
        ),
        (json!({"code": 502}), Some(502), "HTTP 502 (unavailable)"),
        (
            json!({"code": "502", "message": "Provider returned error"}),
            Some(502),
            "HTTP 502 (unavailable): Provider returned error",
        ),
        (json!({"code": 429}), Some(429), "HTTP 429 (rate limited)"),
        (
            json!({"code": "429", "message": "Rate limit exceeded"}),
            Some(429),
            "HTTP 429 (rate limited): Rate limit exceeded",
        ),
        (
            json!({"message": "error code: 429"}),
            Some(429),
            "HTTP 429 (rate limited)",
        ),
        // Exhausted quota is not a rate limit, whatever its status says.
        (
            json!({"code": 429, "type": "insufficient_quota"}),
            Some(429),
            "HTTP 429: type=insufficient_quota",
        ),
        // A status that names no refusal keeps no class.
        (json!({"code": 401}), Some(401), "HTTP 401"),
        // A message alone names no status; it stays the whole account.
        (
            json!({"message": "Provider returned error"}),
            None,
            "Provider returned error",
        ),
        (
            json!({"message": "Provider returned error", "metadata": {"provider_name": "Upstream"}}),
            None,
            "Provider returned error (from Upstream)",
        ),
    ];
    for status in 520_u16..=524 {
        cases.push((
            json!({"message": format!("error code: {status}")}),
            Some(status),
            "",
        ));
        cases.push((json!(format!("error code: {status}")), Some(status), ""));
        cases.push((
            json!({"code": status, "message": "origin unreachable"}),
            Some(status),
            "",
        ));
    }
    for (payload, status, expected) in cases {
        let error = projector()
            .push(&data(&json!({ "error": payload })))
            .expect_err("in-band error");
        assert_eq!(
            error.kind(),
            ChatStreamErrorKind::ProviderError,
            "{payload}"
        );
        assert_eq!(error.http_status(), status, "{payload}");
        let expected = match (expected, status) {
            ("", Some(status)) if payload.get("code").is_some() => {
                format!("HTTP {status} (unavailable): origin unreachable")
            }
            ("", Some(status)) => format!("HTTP {status} (unavailable)"),
            (expected, _) => expected.to_owned(),
        };
        assert_eq!(error.in_band_detail(), Some(expected.as_str()), "{payload}");
        assert_eq!(
            error.message(),
            format!(
                "Chat provider returned an in-band error after accepting the request: {expected}"
            ),
        );
    }
    // Grammar violations are not provider errors and carry no account.
    let violation = projector()
        .push(b"data: {\"object\":\"chat.completion.chunk\"}\n\n")
        .expect_err("missing id");
    assert_eq!(violation.kind(), ChatStreamErrorKind::InvalidResponse);
    assert_eq!(violation.in_band_detail(), None);
}

/// Provider text cannot reorder or hide the operator text around it.
#[test]
fn in_band_text_drops_bidirectional_and_zero_width_format_characters() {
    let error = projector()
        .push(&data(&json!({
            "error": {
                "code": 502,
                "message": "up\u{202E}stream\u{200B} failed\u{2066}",
                "metadata": {"provider_name": "\u{FEFF}Up\u{202D}stream\u{2069}"}
            }
        })))
        .expect_err("in-band error");
    assert_eq!(
        error.in_band_detail(),
        Some("HTTP 502 (unavailable) from Upstream: upstream failed")
    );
}

/// The field shape behind this rule: an accepted OpenRouter stream of
/// keep-alives, then one chunk with no choices that carries the upstream's
/// timeout and names it at the chunk level too.
#[test]
fn an_openrouter_in_band_timeout_after_keepalives_names_its_upstream() {
    let mut openrouter = projector_with_binding(
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1),
    );
    let keepalives = openrouter
        .push(b": OPENROUTER PROCESSING\n\n: OPENROUTER PROCESSING\n\n")
        .expect("keep-alives");
    assert!(
        matches!(keepalives.as_slice(), []),
        "keep-alives project nothing"
    );
    let error = openrouter
        .push(&data(&json!({
            "id": "gen-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "model-literal",
            "provider": "Upstream",
            "choices": [],
            "error": {"code": 504, "message": "error code: 504"}
        })))
        .expect_err("in-band timeout");
    assert_eq!(error.kind(), ChatStreamErrorKind::ProviderError);
    assert_eq!(error.http_status(), Some(504));
    assert_eq!(
        error.in_band_detail(),
        Some("HTTP 504 (timeout) from Upstream")
    );
}

#[test]
fn rejects_usage_mismatch_unsupported_finish_and_provider_error() {
    const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
    let mut bad_usage = usage();
    bad_usage["usage"]["total_tokens"] = json!(99);
    let mut bad_usage_projector = projector();
    push_json(&mut bad_usage_projector, role());
    push_json(
        &mut bad_usage_projector,
        chunk(json!([{"index":0,"delta":{"content":"x"},"finish_reason":null}])),
    );
    push_json(&mut bad_usage_projector, finish("stop"));
    assert_eq!(
        bad_usage_projector
            .push(&data(&bad_usage))
            .expect_err("accounting")
            .kind(),
        ChatStreamErrorKind::InvalidResponse
    );

    let mut unsupported = projector();
    push_json(&mut unsupported, role());
    assert_eq!(
        unsupported
            .push(&data(&finish("other")))
            .expect_err("finish")
            .kind(),
        ChatStreamErrorKind::UnsupportedSemantic
    );

    let error = projector()
        .push(&data(&json!({"error":{"code":"failure"}})))
        .expect_err("provider error");
    assert_eq!(error.kind(), ChatStreamErrorKind::ProviderError);
    assert_eq!(
        error.message(),
        "Chat provider returned an in-band error after accepting the request: code=failure"
    );
    assert_eq!(error.in_band_detail(), Some("code=failure"));

    let nested = projector()
        .push(&data(&json!({
            "error": {"message": "Provider returned error", "code": 502}
        })))
        .expect_err("provider message");
    assert_eq!(
        nested.message(),
        "Chat provider returned an in-band error after accepting the request: \
         HTTP 502 (unavailable): Provider returned error"
    );

    let empty = projector()
        .push(&data(&json!({"error":{}})))
        .expect_err("empty provider error");
    assert_eq!(
        empty.message(),
        "Chat provider returned an in-band error after accepting the request: unclassified"
    );

    let redacted = projector()
        .push(&data(&json!({
            "error": {
                "code": "auth_error",
                "message": format!("Bearer {SECRET_SENTINEL}")
            }
        })))
        .expect_err("credential-bearing provider error");
    assert_eq!(redacted.kind(), ChatStreamErrorKind::ProviderError);
    assert!(
        !redacted.message().contains(SECRET_SENTINEL),
        "in-band Chat error must not retain credential forms: {}",
        redacted.message()
    );
    assert!(redacted.message().contains("[redacted]"));
}

#[test]
fn enforces_output_argument_event_and_sse_bounds() {
    let nz = |value| NonZeroUsize::new(value).expect("nonzero");
    let output_limits =
        ChatStreamLimits::new(nz(1024), nz(4096), nz(20), nz(1), nz(2), nz(2), nz(1 << 20));
    let mut output = projector_with_limits(output_limits);
    push_json(&mut output, role());
    assert_eq!(
        output
            .push(&data(&chunk(
                json!([{"index":0,"delta":{"content":"xx"},"finish_reason":null}])
            )))
            .expect_err("output bound")
            .kind(),
        ChatStreamErrorKind::ResourceLimit
    );

    let argument_limits = ChatStreamLimits::new(
        nz(1024),
        nz(4096),
        nz(20),
        nz(100),
        nz(1),
        nz(2),
        nz(1 << 20),
    );
    let mut arguments = projector_with_limits(argument_limits);
    push_json(&mut arguments, role());
    assert_eq!(arguments.push(&data(&chunk(json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"lookup","arguments":"{}x"}}]},"finish_reason":null}])))).expect_err("argument bound").kind(), ChatStreamErrorKind::ResourceLimit);

    let line_limits =
        ChatStreamLimits::new(nz(4), nz(32), nz(1), nz(10), nz(1), nz(10), nz(1 << 20));
    assert_eq!(
        projector_with_limits(line_limits)
            .push(b"data: too long")
            .expect_err("line bound")
            .kind(),
        ChatStreamErrorKind::ResourceLimit
    );
}

#[test]
fn total_stream_byte_bound_counts_comments_and_spans_tool_calls() {
    let nz = |value| NonZeroUsize::new(value).expect("nonzero");
    let limits = ChatStreamLimits::new(
        nz(1024),
        nz(4096),
        nz(20),
        nz(1024),
        nz(4),
        nz(1024),
        nz(600),
    );
    let mut comments = projector_with_limits(limits);
    let keepalive = b": keepalive\n\n".repeat(50);
    assert!(keepalive.len() > 600);
    assert_eq!(
        comments
            .push(&keepalive)
            .expect_err("comment trickle exceeds the total byte bound")
            .kind(),
        ChatStreamErrorKind::ResourceLimit
    );

    let mut calls = projector_with_limits(limits);
    push_json(&mut calls, role());
    let mut error = None;
    for index in 0..4 {
        let pushed = calls.push(&data(&chunk(json!([{"index":0,"delta":{"tool_calls":[{"index":index,"id":format!("c{index}"),"type":"function","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]))));
        if let Err(failure) = pushed {
            error = Some(failure);
            break;
        }
    }
    assert_eq!(
        error
            .expect("aggregate tool calls exceed the total byte bound")
            .kind(),
        ChatStreamErrorKind::ResourceLimit
    );
}

#[test]
fn maps_part_indexed_tool_slots_to_dense_ordinals() {
    let mut projector = projector();
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"reasoning": "plan"}, "finish_reason": null}])),
    );
    push_json(
        &mut projector,
        chunk(
            json!([{"index": 0, "delta": {"content": "I'll create a file"}, "finish_reason": null}]),
        ),
    );
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 2, "id": "call_a1", "type": "function", "function": {"name": "write_file", "arguments": ""}}
        ]}, "finish_reason": null}])),
    );
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 2, "function": {"arguments": "{\"path\":\"hello.txt\","}}
        ]}, "finish_reason": null}])),
    );
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 2, "function": {"arguments": "\"content\":\"hi\"}"}}
        ]}, "finish_reason": null}])),
    );
    complete(&mut projector, "tool_calls");

    let terminal = projector.finish().expect("terminal");
    let blocks = terminal.response().output()[0].blocks();
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[0], ContentBlock::Reasoning(_)));
    assert!(matches!(&blocks[1], ContentBlock::Text(_)));
    assert!(matches!(&blocks[2], ContentBlock::ToolCall(call)
            if call.exposed_name().as_str() == "write_file"
            && call.raw_arguments() == "{\"path\":\"hello.txt\",\"content\":\"hi\"}"
            && call.correlation().remote_call_id() == Some("call_a1")));
}

#[test]
fn maps_sparse_tool_index_reorder_by_first_seen_ordinal() {
    let mut projector = projector();
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 3, "id": "call-b", "type": "function", "function": {"name": "second_tool", "arguments": "{}"}}
        ]}, "finish_reason": null}])),
    );
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 2, "id": "call-a", "type": "function", "function": {"name": "first_tool", "arguments": "{}"}}
        ]}, "finish_reason": null}])),
    );
    complete(&mut projector, "tool_calls");

    let terminal = projector.finish().expect("terminal");
    let blocks = terminal.response().output()[0].blocks();
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], ContentBlock::ToolCall(call)
            if call.exposed_name().as_str() == "second_tool"
            && call.correlation().remote_call_id() == Some("call-b")));
    assert!(matches!(&blocks[1], ContentBlock::ToolCall(call)
            if call.exposed_name().as_str() == "first_tool"
            && call.correlation().remote_call_id() == Some("call-a")));
}

#[test]
fn rejects_empty_tool_call_id_or_name() {
    let mut empty_id_projector = projector();
    push_json(&mut empty_id_projector, role());
    assert_eq!(
        push_chunk(
            &mut empty_id_projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
            ]}, "finish_reason": null}]))
        )
        .expect_err("empty id")
        .kind(),
        ChatStreamErrorKind::InvalidResponse
    );

    let mut empty_name_projector = projector();
    push_json(&mut empty_name_projector, role());
    assert_eq!(
        push_chunk(
            &mut empty_name_projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-a", "type": "function", "function": {"name": "", "arguments": "{}"}}
            ]}, "finish_reason": null}]))
        )
        .expect_err("empty name")
        .kind(),
        ChatStreamErrorKind::InvalidResponse
    );
}

#[test]
fn projects_unadvertised_tool_name_and_refuses_invalid_spelling() {
    // A name outside the prepared catalog is not a wire fault: the tool runtime
    // answers the model with a recoverable UnknownTool result on the next turn.
    let mut unadvertised = projector();
    push_json(&mut unadvertised, role());
    push_json(
        &mut unadvertised,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call-x", "type": "function", "function": {"name": "not_a_tool", "arguments": "{\"path\":\"a\"}"}}
        ]}, "finish_reason": null}])),
    );
    complete(&mut unadvertised, "tool_calls");
    let terminal = unadvertised.finish().expect("terminal");
    let blocks = terminal.response().output()[0].blocks();
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], ContentBlock::ToolCall(call)
            if call.exposed_name().as_str() == "not_a_tool"
            && call.raw_arguments() == "{\"path\":\"a\"}"
            && call.correlation().remote_call_id() == Some("call-x")));

    // Syntactically impossible names remain a wire fault.
    let mut invalid = projector();
    push_json(&mut invalid, role());
    push_json(
        &mut invalid,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call-y", "type": "function", "function": {"name": "not a tool", "arguments": "{}"}}
        ]}, "finish_reason": null}])),
    );
    push_json(&mut invalid, finish("tool_calls"));
    push_json(&mut invalid, usage());
    let error = invalid.push(b"data: [DONE]\n\n").expect_err("invalid name");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    assert!(error.message().contains("invalid tool name"));

    // A non-object argument payload remains a wire fault.
    let mut non_object = projector();
    push_json(&mut non_object, role());
    push_json(
        &mut non_object,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call-z", "type": "function", "function": {"name": "not_a_tool", "arguments": "[]"}}
        ]}, "finish_reason": null}])),
    );
    push_json(&mut non_object, finish("tool_calls"));
    push_json(&mut non_object, usage());
    let error = non_object
        .push(b"data: [DONE]\n\n")
        .expect_err("non-object arguments");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    assert!(
        error
            .message()
            .contains("tool arguments must be one JSON object")
    );
}

#[test]
fn maps_parallel_part_indexed_tool_slots() {
    let mut projector = projector();
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"content": "go"}, "finish_reason": null}])),
    );
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 2, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":"}},
            {"index": 3, "id": "call-b", "type": "function", "function": {"name": "write", "arguments": "{\"n\":"}}
        ]}, "finish_reason": null}])),
    );
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 2, "function": {"arguments": "\"x\"}"}},
            {"index": 3, "function": {"arguments": "2}"}}
        ]}, "finish_reason": null}])),
    );
    complete(&mut projector, "tool_calls");

    let terminal = projector.finish().expect("terminal");
    let blocks = terminal.response().output()[0].blocks();
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[1], ContentBlock::ToolCall(call)
            if call.exposed_name().as_str() == "lookup"
            && call.correlation().remote_call_id() == Some("call-a")));
    assert!(matches!(&blocks[2], ContentBlock::ToolCall(call)
            if call.exposed_name().as_str() == "write"
            && call.correlation().remote_call_id() == Some("call-b")));
}

#[test]
fn admits_tool_call_when_name_arrives_in_second_delta() {
    let mut projector = projector();
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call-a", "type": "function", "function": {"arguments": "{\"q\":"}}
        ]}, "finish_reason": null}])),
    );
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"name": "lookup", "arguments": "\"x\"}"}}
        ]}, "finish_reason": null}])),
    );
    complete(&mut projector, "tool_calls");

    let terminal = projector.finish().expect("terminal");
    let blocks = terminal.response().output()[0].blocks();
    assert!(matches!(&blocks[0], ContentBlock::ToolCall(call)
            if call.exposed_name().as_str() == "lookup"
            && call.raw_arguments() == "{\"q\":\"x\"}"
            && call.correlation().remote_call_id() == Some("call-a")));
}

#[test]
fn rejects_tool_slot_without_name_at_finish() {
    let mut projector = projector();
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call-a", "type": "function", "function": {"arguments": "{}"}}
        ]}, "finish_reason": null}])),
    );
    push_json(&mut projector, finish("tool_calls"));
    push_json(&mut projector, usage());
    let error = projector
        .push(b"data: [DONE]\n\n")
        .expect_err("missing name");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    assert!(
        error
            .message()
            .contains("tool-call slot never bound id and name")
    );
}

#[test]
fn rejects_repeated_tool_call_id_across_slots() {
    let mut projector = projector();
    push_json(&mut projector, role());
    let error = projector
        .push(&data(&chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{}"}},
            {"index": 1, "id": "call-a", "type": "function", "function": {"name": "write", "arguments": "{}"}}
        ]}, "finish_reason": null}]))))
        .expect_err("repeated id");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    assert!(error.message().contains("tool-call id repeated"));
}

#[test]
fn rejects_tool_call_slot_index_at_bound() {
    let mut projector = projector();
    push_json(&mut projector, role());
    let error = projector
        .push(&data(&chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 4096, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
        ]}, "finish_reason": null}]))))
        .expect_err("index bound");
    assert_eq!(error.kind(), ChatStreamErrorKind::ResourceLimit);
    assert!(
        error
            .message()
            .contains("tool-call index exceeded its bound")
    );
}

#[test]
fn rejects_repeated_finish_reason_chunk_without_usage() {
    let openrouter_binding =
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1);
    let mut projector = projector_with_binding(openrouter_binding);
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"content": "answer"}, "finish_reason": null}])),
    );
    push_json(&mut projector, finish("stop"));
    let error = projector
        .push(&data(&chunk(json!([{
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
            "native_finish_reason": "stop"
        }]))))
        .expect_err("second bare finish");
    assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    assert!(
        error
            .message()
            .contains("choice index or lifecycle is invalid")
    );
}

#[test]
fn accepts_matching_finish_reason_in_usage_chunk() {
    let openrouter_binding =
        binding(ChatStreamLimits::default()).with_extensions(ChatStreamExtensions::OpenRouterV1);
    let mut projector = projector_with_binding(openrouter_binding);
    push_json(&mut projector, role());
    push_json(
        &mut projector,
        chunk(json!([{"index": 0, "delta": {"content": "answer"}, "finish_reason": null}])),
    );
    push_json(&mut projector, finish("stop"));
    let mut terminal_usage = usage();
    terminal_usage["choices"] = json!([{
        "index": 0,
        "delta": {},
        "finish_reason": "stop",
        "native_finish_reason": "stop"
    }]);
    push_json(&mut projector, terminal_usage);
    projector.push(b"data: [DONE]\n\n").expect("done");
    let terminal = projector.finish().expect("terminal");
    assert_eq!(terminal.finish().canonical(), FinishReason::Stop);
}
