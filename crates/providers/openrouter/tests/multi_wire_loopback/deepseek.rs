use super::*;
use plakous_model::{
    ConfiguredLanguageModel, ConfiguredTool, ToolCallerDisposition, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolExecutionDisposition, ToolInputContract, ToolOutputContract,
};
use serde_json::{Value, json};
use std::fmt::Write as _;

const DEEPSEEK: &str = "deepseek/deepseek-v4-flash-0731";

fn lookup() -> TestResult<ConfiguredTool> {
    Ok(ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("lookup")?,
            "1.0.0",
            "Look up a value",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object", "properties": {"q": {"type": "string"}},
                "required": ["q"], "additionalProperties": false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?,
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    ))
}

fn tool_stream(alias: &str) -> TestResult<String> {
    let mut stream = ": OPENROUTER PROCESSING\n\n".to_owned();
    for (delta, finish) in [
        (json!({"role": "assistant"}), Value::Null),
        (json!({alias: "Check the lookup."}), Value::Null),
        (
            json!({"tool_calls": [{"index": 0, "id": "call-deepseek", "type": "function",
            "function": {"name": "lookup", "arguments": "{\"q\":"}}]}),
            Value::Null,
        ),
        (
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"café\"}"}}]}),
            Value::Null,
        ),
        (json!({}), json!("tool_calls")),
    ] {
        write!(
            stream,
            "data: {}\n\n",
            json!({
                "id": "gen-deepseek", "object": "chat.completion.chunk", "created": 1_787_623_695,
                "model": DEEPSEEK, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
            })
        )?;
    }
    write!(
        stream,
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": "gen-deepseek", "object": "chat.completion.chunk", "created": 1_787_623_695,
            "model": DEEPSEEK, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20,
                      "completion_tokens_details": {"reasoning_tokens": 3}}
        })
    )?;
    Ok(stream)
}

fn tool_request(
    configured: &ConfiguredLanguageModel,
    input: ConversationView,
    ordinal: u128,
) -> TestResult<ModelRequest> {
    let initial = request(configured, ordinal)?;
    Ok(ModelRequest::builder(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        input,
        initial.metadata(),
    )
    .tools(vec![lookup()?])
    .tool_choice(ToolChoice::Auto)
    .limits(initial.limits().clone())
    .resolved_harness_plan_identity(initial.resolved_harness_plan_identity().ok_or("plan")?)
    .build()?)
}

#[tokio::test]
async fn deepseek_streamed_tools_and_both_reasoning_aliases_round_trip_on_chat() -> TestResult {
    for alias in ["reasoning", "reasoning_content"] {
        let mut replies = discovery_replies();
        for (_, body, _) in &mut replies {
            *body = body.replace(MODEL, DEEPSEEK);
        }
        replies.extend([
            (
                "text/event-stream",
                tool_stream(alias)?,
                "x-request-id: req-deepseek-tool\r\n",
            ),
            (
                "text/event-stream",
                chat_sse().replace(MODEL, DEEPSEEK),
                "x-request-id: req-deepseek-done\r\n",
            ),
        ]);
        let (client, origin, peer) = loopback(b"deepseek-chat-tool-round-trip", replies).await?;
        let mut selected = behavior("openrouter.chat.dynamic-v1", "openrouter-chat-retention-v1")?;
        selected.tools = vec![lookup()?];
        selected.tool_choice = ToolChoice::Auto;
        let configured = discovered_chat_model_with(client, origin, DEEPSEEK, selected).await?;
        let initial = request(&configured, 100)?;
        let mut input = initial.input().items().to_vec();
        let events = execute_request(
            &configured,
            tool_request(&configured, initial.input().clone(), 100)?,
        )
        .await?;
        let response = events
            .iter()
            .find_map(|event| match event {
                ModelEvent::ResponseCompleted(response) => Some(response),
                _ => None,
            })
            .ok_or("tool response")?;
        let call = response
            .output()
            .iter()
            .flat_map(ConversationItem::blocks)
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .ok_or("tool call")?;
        assert_eq!(call.arguments(), Some(&json!({"q": "café"})));
        input.extend_from_slice(response.output());
        input.push(ConversationItem::new(
            plakous_model::EntryId::from_str("00000000-0000-4000-8000-000000000501")?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock::new(
                call.call_id(),
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
                call.correlation().clone(),
            )?)],
            Provenance::ToolExecution,
        )?);
        let events = execute_request(
            &configured,
            tool_request(&configured, ConversationView::from_items(input)?, 200)?,
        )
        .await?;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
                .count(),
            1
        );
        assert_round_trip_requests(&peer.await??)?;
    }
    Ok(())
}

fn assert_round_trip_requests(requests: &[Vec<u8>]) -> TestResult {
    assert_eq!(requests.len(), 4);
    for request in &requests[2..] {
        assert!(String::from_utf8_lossy(request).starts_with("POST /api/v1/chat/completions "));
    }
    let follow_up = std::str::from_utf8(&requests[3])?;
    let body: Value = serde_json::from_str(follow_up.split_once("\r\n\r\n").ok_or("body")?.1)?;
    assert_eq!(body["model"], DEEPSEEK);
    assert_eq!(body["stream"], true);
    assert_eq!(body["tools"][0]["function"]["name"], "lookup");
    let messages = body["messages"].as_array().ok_or("messages")?;
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .ok_or("assistant")?;
    assert_eq!(assistant["tool_calls"][0]["id"], "call-deepseek");
    assert_eq!(assistant["reasoning"], "Check the lookup.");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .ok_or("result")?;
    assert_eq!(result["tool_call_id"], "call-deepseek");
    assert_eq!(result["content"], "found");
    Ok(())
}
