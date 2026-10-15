//! The tool-free operation-budget wrap-up through `prepare_call` on SuperGrok.
//!
//! The wrap-up round only withdraws tool selection. The Responses artifact
//! binding leaves tool selection out of its prefix identity, so the wrap-up
//! keeps the live artifact's bound replay and the next turn binds the
//! wrap-up's own artifact. Any other stable-prefix difference lowers fresh.

use plakous_model::{InstructionBlock, InstructionSource, ToolChoice};

use super::*;

const MODEL_REPORTED: &str = "grok-4.6-build";

fn sse(events: Vec<serde_json::Value>) -> String {
    events.into_iter().fold(String::new(), |mut stream, event| {
        stream.push_str("data: ");
        stream.push_str(&event.to_string());
        stream.push_str("\n\n");
        stream
    })
}

fn created(sequence: u32, id: &str) -> serde_json::Value {
    json!({"type":"response.created","sequence_number":sequence,
        "response":{"id":id,"status":"in_progress","model":MODEL_REPORTED,"output":[]}})
}

fn completed(sequence: u32, id: &str, output: Vec<serde_json::Value>) -> serde_json::Value {
    json!({"type":"response.completed","sequence_number":sequence,"response":{
        "id":id,"object":"response","status":"completed","model":MODEL_REPORTED,
        "output":output,
        "usage":{"input_tokens":6,"output_tokens":4,"total_tokens":10},
        "incomplete_details":null,"error":null}})
}

fn reasoning_item(id: &str, encrypted: &str) -> serde_json::Value {
    json!({"id":id,"type":"reasoning","status":"completed","encrypted_content":encrypted,
        "summary":[{"type":"summary_text","text":"I checked."}]})
}

fn text_message(id: &str, text: &str) -> serde_json::Value {
    json!({"id":id,"type":"message","status":"completed","role":"assistant",
        "content":[{"type":"output_text","text":text,"annotations":[]}]})
}

/// Stream events for one completed reasoning item at `index`.
fn reasoning_events(sequence: u32, index: u32, item: &serde_json::Value) -> Vec<serde_json::Value> {
    vec![
        json!({"type":"response.output_item.added","sequence_number":sequence,"output_index":index,
            "item":{"id":item["id"],"type":"reasoning","status":"in_progress","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","sequence_number":sequence + 1,
            "output_index":index,"summary_index":0,"delta":"I checked."}),
        json!({"type":"response.output_item.done","sequence_number":sequence + 2,
            "output_index":index,"item":item}),
    ]
}

/// Stream events for one completed assistant message with one text part.
fn message_events(sequence: u32, index: u32, item: &serde_json::Value) -> Vec<serde_json::Value> {
    let part = &item["content"][0];
    let text = part["text"].clone();
    vec![
        json!({"type":"response.output_item.added","sequence_number":sequence,"output_index":index,
            "item":{"id":item["id"],"type":"message","status":"in_progress","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","sequence_number":sequence + 1,
            "output_index":index,"content_index":0,
            "part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","sequence_number":sequence + 2,
            "output_index":index,"content_index":0,"delta":text}),
        json!({"type":"response.content_part.done","sequence_number":sequence + 3,
            "output_index":index,"content_index":0,"part":part}),
        json!({"type":"response.output_item.done","sequence_number":sequence + 4,
            "output_index":index,"item":item}),
    ]
}

/// Reasoning, an interim message and a function call: one realistic tool round.
fn reasoning_message_function_call_sse() -> String {
    let reasoning = reasoning_item("rs_wrap", "encrypted-wrap-reasoning");
    let message = text_message("msg_wrap", "Checking the fixture first.");
    let call = json!({"id":"fc_wrap","type":"function_call","status":"completed",
        "call_id":"call_wrap","name":"fixture_tool","arguments":"{\"path\":\"fixture.txt\"}"});
    let mut events = vec![created(1, "resp_wrap_tool")];
    events.extend(reasoning_events(2, 0, &reasoning));
    events.extend(message_events(5, 1, &message));
    events.extend([
        json!({"type":"response.output_item.added","sequence_number":10,"output_index":2,
            "item":{"id":"fc_wrap","type":"function_call","status":"in_progress",
                "call_id":"call_wrap","name":"fixture_tool","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","sequence_number":11,
            "output_index":2,"item_id":"fc_wrap","delta":"{\"path\":\"fixture.txt\"}"}),
        json!({"type":"response.function_call_arguments.done","sequence_number":12,
            "output_index":2,"item_id":"fc_wrap","arguments":"{\"path\":\"fixture.txt\"}"}),
        json!({"type":"response.output_item.done","sequence_number":13,"output_index":2,
            "item":call.clone()}),
        completed(14, "resp_wrap_tool", vec![reasoning, message, call]),
    ]);
    sse(events)
}

/// Reasoning, a message, a retained unknown output item and a refusal: a
/// history fresh lowering refuses as `ProtectedHistory`.
fn protected_history_sse() -> String {
    let reasoning = reasoning_item("rs_protected", "encrypted-protected-reasoning");
    let message = text_message("msg_protected", "Partial answer.");
    let unknown = json!({"id":"future_protected","type":"future_output","status":"completed",
        "payload":{"opaque":"future-payload"}});
    let refusal = json!({"id":"refusal_protected","type":"message","status":"completed",
        "role":"assistant","content":[{"type":"refusal","refusal":"I cannot share that."}]});
    let mut events = vec![created(1, "resp_protected")];
    events.extend(reasoning_events(2, 0, &reasoning));
    events.extend(message_events(5, 1, &message));
    events.extend([
        json!({"type":"response.output_item.added","sequence_number":10,"output_index":2,
            "item":{"id":"future_protected","type":"future_output","status":"in_progress",
                "payload":{"opaque":"future-payload"}}}),
        json!({"type":"response.output_item.done","sequence_number":11,"output_index":2,
            "item":unknown.clone()}),
        json!({"type":"response.output_item.added","sequence_number":12,"output_index":3,
            "item":{"id":"refusal_protected","type":"message","status":"in_progress",
                "role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","sequence_number":13,"output_index":3,
            "content_index":0,"part":{"type":"refusal","refusal":"I cannot share that."}}),
        json!({"type":"response.content_part.done","sequence_number":14,"output_index":3,
            "content_index":0,"part":{"type":"refusal","refusal":"I cannot share that."}}),
        json!({"type":"response.output_item.done","sequence_number":15,"output_index":3,
            "item":refusal.clone()}),
        completed(
            16,
            "resp_protected",
            vec![reasoning, message, unknown, refusal],
        ),
    ]);
    sse(events)
}

fn native_ok_sse() -> String {
    success_sse().replace("grok-4.5", MODEL_REPORTED)
}

fn user_item(entry: &str, text: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        EntryId::from_str(entry)?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(text, None)?)],
        Provenance::UserInput,
    )?)
}

/// The ordinary tool round's request with the given tool selection and
/// instructions; every other stable-prefix field is the live one.
fn round_request(
    input: ConversationView,
    attempt: &str,
    tool_choice: ToolChoice,
    instructions: Vec<InstructionBlock>,
) -> TestResult<ModelRequest> {
    let (selected, effective) = provider_configuration_ids()?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        input,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000425")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000426")?,
            AttemptId::from_str(attempt)?,
            now_timestamp()?,
        ),
    )
    .limits(test_generation_limits()?)
    .tools(vec![plakous_model::ConfiguredTool::new(
        fixture_tool()?,
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectOnly,
    )])
    .instructions(instructions)
    .tool_choice(tool_choice)
    .resolved_harness_plan_identity(test_harness_plan_identity()?)
    .build()?)
}

async fn subscription_model(
    temp: &TempDir,
    auth: &MockPeer,
    inference: &MockPeer,
    subject: &str,
) -> TestResult<(XaiNativeAuthManager, Arc<XaiLanguageModel>)> {
    let manager = manager(temp, auth)?;
    install_stored(
        &manager,
        subject,
        &format!("{subject}-access"),
        &format!("{subject}-refresh"),
        system_unix_seconds()? + 3600,
    )?;
    admit_catalog(&manager, auth).await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_46_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;
    Ok((manager, model))
}

/// Prepares one round beside `artifact` and drives it to its terminal response.
async fn prepared_round(
    model: &Arc<XaiLanguageModel>,
    request: ModelRequest,
    attempt: &str,
    artifact: ActiveFamilyArtifact,
) -> TestResult<ModelResponse> {
    let integrity = artifact.integrity();
    let prepared = prepare_model_call(Arc::clone(model), request, Some(artifact))
        .await
        .map_err(|error| std::io::Error::other(error.message().to_owned()))?;
    assert_eq!(
        prepared.record().projection().active_artifact(),
        Some(integrity)
    );
    Ok(complete_prepared_model_round(
        prepared,
        ModelCallContext::new(
            AttemptId::from_str(attempt)?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await
    .map_err(|error| std::io::Error::other(error.message().to_owned()))?)
}

fn input_items(body: &serde_json::Value) -> TestResult<&Vec<serde_json::Value>> {
    Ok(body["input"]
        .as_array()
        .ok_or_else(|| std::io::Error::other("request input was not an array"))?)
}

fn tool_exchange(
    first: &ModelResponse,
    entry: &str,
) -> TestResult<(ConversationItem, Vec<ConversationItem>)> {
    let call = first
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .ok_or_else(|| std::io::Error::other("the tool round did not request the tool"))?;
    let result = ConversationItem::new(
        EntryId::from_str(entry)?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock::new(
            call.call_id(),
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new(
                "fixture result",
                None,
            )?)],
            call.correlation().clone(),
        )?)],
        Provenance::ToolExecution,
    )?;
    Ok((result, first.output().to_vec()))
}

/// The wrap-up over a reasoning, message and function-call round with its
/// result carries `tool_choice: "none"` and replays every native item under the
/// live artifact; an instruction drift instead lowers every portable item
/// fresh, with no encrypted reasoning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_wrap_up_replays_the_live_artifact_and_other_drift_lowers_fresh() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let inference = mock_peer().await?;
    for body in [
        reasoning_message_function_call_sse(),
        native_ok_sse(),
        native_ok_sse(),
    ] {
        inference.responses.send(sse_response(body)).await?;
    }
    let (_manager, model) = subscription_model(&temp, &auth, &inference, "wrap-up-subject").await?;
    let user = user_item("00000000-0000-4000-8000-000000000471", "read the fixture")?;
    let first = complete_model_round(
        Arc::clone(&model),
        round_request(
            ConversationView::from_items(vec![user.clone()])?,
            "00000000-0000-4000-8000-000000000472",
            ToolChoice::Auto,
            Vec::new(),
        )?,
        "00000000-0000-4000-8000-000000000472",
    )
    .await?;
    let live = promote_responses_family_artifact(&first)?;
    let (result, output) = tool_exchange(&first, "00000000-0000-4000-8000-000000000473")?;
    let history = tool_follow_up_view(user, &output, result)?;

    let wrap_up = prepared_round(
        &model,
        round_request(
            history.clone(),
            "00000000-0000-4000-8000-000000000474",
            ToolChoice::None,
            Vec::new(),
        )?,
        "00000000-0000-4000-8000-000000000474",
        live.clone(),
    )
    .await?;
    assert_responses_family_artifact_candidate(&wrap_up)?;
    let drifted = prepared_round(
        &model,
        round_request(
            history,
            "00000000-0000-4000-8000-000000000475",
            ToolChoice::None,
            vec![InstructionBlock::new(
                InstructionSource::System,
                "Answer briefly.",
            )?],
        )?,
        "00000000-0000-4000-8000-000000000475",
        live,
    )
    .await?;
    assert_native_ok(&drifted);

    wait_for_requests(&inference, 3).await?;
    let requests = inference.requests.lock().await;
    let bound = captured_request_body(&requests[1])?;
    assert_eq!(bound["tool_choice"], "none");
    assert_eq!(bound["tools"][0]["name"], "fixture_tool");
    let input = input_items(&bound)?;
    assert_eq!(
        input
            .iter()
            .map(|item| item["type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        [
            "message",
            "reasoning",
            "message",
            "function_call",
            "function_call_output"
        ]
    );
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["encrypted_content"], "encrypted-wrap-reasoning");
    assert_eq!(input[2]["role"], "assistant");
    assert!(input[2].to_string().contains("Checking the fixture first."));
    assert_eq!(input[3]["call_id"], "call_wrap");
    assert_eq!(input[3]["name"], "fixture_tool");
    assert_eq!(input[4]["call_id"], "call_wrap");
    assert!(input[4]["output"].to_string().contains("fixture result"));

    let fresh = captured_request_body(&requests[2])?;
    assert_eq!(fresh["tool_choice"], "none");
    assert!(!fresh.to_string().contains("encrypted-wrap-reasoning"));
    let input = input_items(&fresh)?;
    assert!(
        input
            .iter()
            .all(|item| item.get("encrypted_content").is_none())
    );
    for (kind, needle) in [
        ("message", "read the fixture"),
        ("message", "Checking the fixture first."),
        ("function_call", "call_wrap"),
        ("function_call_output", "fixture result"),
    ] {
        assert!(
            input
                .iter()
                .any(|item| item["type"] == kind && item.to_string().contains(needle)),
            "fresh lowering lost the portable {kind} item carrying {needle}"
        );
    }
    drop(requests);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

/// A wrap-up over a history holding a retained unknown output item and an
/// assistant refusal, which fresh lowering refuses as `ProtectedHistory`,
/// keeps bound replay; the next turn's first round binds the wrap-up's own
/// artifact and still replays both items.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_wrap_up_over_protected_history_keeps_bound_replay_into_the_next_turn() -> TestResult
{
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let inference = mock_peer().await?;
    for body in [protected_history_sse(), native_ok_sse(), native_ok_sse()] {
        inference.responses.send(sse_response(body)).await?;
    }
    let (_manager, model) =
        subscription_model(&temp, &auth, &inference, "protected-subject").await?;
    let user = user_item("00000000-0000-4000-8000-000000000481", "summarize the file")?;
    let first = complete_model_round(
        Arc::clone(&model),
        round_request(
            ConversationView::from_items(vec![user.clone()])?,
            "00000000-0000-4000-8000-000000000482",
            ToolChoice::Auto,
            Vec::new(),
        )?,
        "00000000-0000-4000-8000-000000000482",
    )
    .await
    .map_err(|error| {
        let message = error
            .downcast_ref::<ModelError>()
            .map_or_else(|| error.to_string(), |model| model.message().to_string());
        std::io::Error::other(format!("first round: {message}"))
    })?;
    let live = promote_responses_family_artifact(&first)?;
    let mut segments = vec![
        HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user])?,
        HistorySegment::new(HistorySegmentKind::ReasoningReplay, first.output().to_vec())?,
    ];
    let wrap_up_request = round_request(
        ConversationView::from_shared(
            CanonicalHistory::new(segments.clone())?,
            WorkingProjection::default(),
        ),
        "00000000-0000-4000-8000-000000000483",
        ToolChoice::None,
        Vec::new(),
    )?;
    assert!(
        prepare_model_call(Arc::clone(&model), wrap_up_request.clone(), None)
            .await
            .is_err(),
        "this history must stay unpreparable without the bound replay"
    );
    let wrap_up = prepared_round(
        &model,
        wrap_up_request,
        "00000000-0000-4000-8000-000000000483",
        live,
    )
    .await?;
    let wrap_up_artifact = promote_responses_family_artifact(&wrap_up)?;

    segments.push(HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        wrap_up.output().to_vec(),
    )?);
    segments.push(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![user_item(
            "00000000-0000-4000-8000-000000000484",
            "next question",
        )?],
    )?);
    let next = prepared_round(
        &model,
        round_request(
            ConversationView::from_shared(
                CanonicalHistory::new(segments)?,
                WorkingProjection::default(),
            ),
            "00000000-0000-4000-8000-000000000485",
            ToolChoice::Auto,
            Vec::new(),
        )?,
        "00000000-0000-4000-8000-000000000485",
        wrap_up_artifact,
    )
    .await?;
    assert_native_ok(&next);

    wait_for_requests(&inference, 3).await?;
    let requests = inference.requests.lock().await;
    let wrap_up_body = captured_request_body(&requests[1])?;
    let next_body = captured_request_body(&requests[2])?;
    assert_eq!(wrap_up_body["tool_choice"], "none");
    assert_eq!(next_body["tool_choice"], "auto");
    for body in [&wrap_up_body, &next_body] {
        let input = input_items(body)?;
        assert!(input.iter().any(|item| {
            item["type"] == "reasoning"
                && item["encrypted_content"] == "encrypted-protected-reasoning"
        }));
        assert!(input.iter().any(|item| {
            item["type"] == "future_output" && item.to_string().contains("future-payload")
        }));
        assert!(input.iter().any(|item| {
            item["type"] == "message"
                && item["role"] == "assistant"
                && item.to_string().contains("I cannot share that.")
        }));
    }
    assert!(
        input_items(&next_body)?
            .iter()
            .any(|item| { item["type"] == "message" && item.to_string().contains("native ok") })
    );
    drop(requests);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}
