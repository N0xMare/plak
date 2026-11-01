use super::*;
use crate::MessagesStopReason;
use plakous_model::{ModelResponse, ModelResponseAssembler};

const COMPLETE_INPUT: &str = r#"{"path":"notes.txt","content":"all of it"}"#;
const TRUNCATED_INPUT: &str = r#"{"path":"notes.txt","content":"cut mid-str"#;

fn sse(event: &str, data: &serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// A text block, then one client `tool_use` whose input arrives as
/// `input_json_delta`, both closed before the terminal message metadata.
fn tool_stream(input: &str, reason: &str, details: Option<serde_json::Value>) -> String {
    let start = message_start_stream("");
    let mut stream = format!("{}\n\n", start.split("\n\n").next().expect("start"));
    for (event, data) in [
        (
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "Writing the file now."}}),
        ),
        (
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 1,
                "content_block": {"type": "tool_use", "id": "toolu_cut", "name": "write_file", "input": {}}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": ""}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": input}}),
        ),
        (
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 1}),
        ),
    ] {
        stream.push_str(&sse(event, &data));
    }
    let mut delta = serde_json::json!({"stop_reason": reason, "stop_sequence": null});
    if let Some(details) = details {
        delta["stop_details"] = details;
    }
    stream.push_str(&sse(
        "message_delta",
        &serde_json::json!({"type": "message_delta", "delta": delta, "usage": {"output_tokens": 128}}),
    ));
    stream.push_str(&sse(
        "message_stop",
        &serde_json::json!({"type": "message_stop"}),
    ));
    stream
}

fn project(
    mut projector: MessagesStreamProjector,
    stream: &str,
) -> Result<(ModelResponse, Vec<ModelEvent>, MessagesStopReason), crate::MessagesStreamError> {
    let mut stream = stream.to_owned();
    if projector.prepared.dialect.extensions() == MessagesStreamExtensions::OpenRouterV1 {
        stream.push_str("event: data\ndata: [DONE]\n\n");
    }
    let mut events = Vec::new();
    for chunk in stream.as_bytes().chunks(11) {
        events.extend(projector.push(chunk)?);
    }
    events.extend(projector.finish_eof()?);
    let mut assembler =
        ModelResponseAssembler::new(projector.prepared.plan_digest()).expect("assembler");
    let mut terminal_response = None;
    for event in events.iter().cloned() {
        if let Some(response) = assembler.push(event).expect("canonical revoked events") {
            terminal_response = Some(response);
        }
    }
    let (response, _, stop) = projector.into_terminal_projection()?.into_parts();
    assert_eq!(terminal_response.as_ref(), Some(&response));
    Ok((response, events, stop.reason()))
}

/// Anthropic documents that a `max_tokens` stop can end inside a `tool_use`
/// block. Neither a complete nor a cut input becomes a canonical call: the
/// terminal is an empty `Length`, which the driver classifies as OutputLimit.
#[test]
fn output_limit_revokes_complete_and_truncated_client_tool_input() {
    for (reason, details) in [
        ("max_tokens", None),
        ("model_context_window_exceeded", None),
        (
            "model_context_window_exceeded",
            Some(serde_json::json!({"type": "model_context_window_exceeded"})),
        ),
    ] {
        for input in [COMPLETE_INPUT, TRUNCATED_INPUT] {
            for projector in [
                projector(),
                claude_oauth_projector(),
                openrouter_projector(),
            ] {
                let (response, events, stop) =
                    project(projector, &tool_stream(input, reason, details.clone()))
                        .expect("output-limit terminal");
                assert!(matches!(
                    stop,
                    MessagesStopReason::MaxTokens | MessagesStopReason::ModelContextWindowExceeded
                ));
                assert_eq!(response.finish().reason(), FinishReason::Length);
                assert!(
                    response
                        .finish()
                        .detail()
                        .expect("finish detail")
                        .starts_with(&format!("stop_reason={reason}"))
                );
                assert_eq!(response.output(), []);
                assert!(response.usage().is_some());
                assert!(response.response_id().is_some());
                assert!(response.validate_for_commit().is_ok());
                let tool_location = plakous_model::OutputLocation::new(0, 1);
                assert!(
                    !events.iter().any(|event| matches!(
                        event,
                        ModelEvent::BlockCompleted(completed)
                            if completed.location() == tool_location
                    )),
                    "a revoked proposal never completes"
                );
            }
        }
    }
}

/// Without client tool input an output-limit stop keeps its visible output,
/// so the existing truncated-answer path is unchanged.
#[test]
fn output_limit_without_client_tool_keeps_visible_output() {
    let stream = refusal_stream(
        Some(r#"{"type":"text","text":"partial answer"}"#),
        "max_tokens",
        None,
    );
    let (response, _, _) = project(projector(), &stream).expect("length terminal");
    assert_eq!(response.finish().reason(), FinishReason::Length);
    assert!(matches!(
        response.output()[0].blocks(),
        [ContentBlock::Text(text)] if text.text() == "partial answer"
    ));
}

#[test]
fn other_terminals_keep_strict_client_tool_validation() {
    for input in [COMPLETE_INPUT, TRUNCATED_INPUT] {
        for reason in ["end_turn", "stop_sequence", "pause_turn"] {
            let stream = tool_stream(input, reason, None).replace(
                r#""stop_reason":"stop_sequence","stop_sequence":null"#,
                r#""stop_reason":"stop_sequence","stop_sequence":"STOP""#,
            );
            let error = projector()
                .push(stream.as_bytes())
                .expect_err("client input under a non-tool terminal");
            let message = error.to_string();
            if input == COMPLETE_INPUT {
                assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
                assert!(
                    message.contains(&format!(
                        "stop_reason={reason} client_proposals=1 provider_proposals=0"
                    )),
                    "{message}"
                );
            } else {
                assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidEvent);
                assert!(message.contains("not complete valid JSON"), "{message}");
            }
        }
    }
    let error = projector()
        .push(tool_stream(TRUNCATED_INPUT, "tool_use", None).as_bytes())
        .expect_err("cut input under tool_use");
    assert!(error.to_string().contains("not complete valid JSON"));
    let (response, _, _) = project(projector(), &tool_stream(COMPLETE_INPUT, "tool_use", None))
        .expect("complete tool_use terminal");
    assert_eq!(response.finish().reason(), FinishReason::ToolCalls);
    assert!(matches!(
        response.output()[0].blocks(),
        [ContentBlock::Text(_), ContentBlock::ToolCall(call)]
            if call.raw_arguments() == COMPLETE_INPUT
    ));
    for (reason, message) in [
        ("tool_use", "client_proposals=0 provider_proposals=0"),
        ("max_tokens", "refusal stop_reason did not match"),
    ] {
        let stream = refusal_stream(
            Some(r#"{"type":"refusal","refusal":"cannot comply"}"#),
            reason,
            None,
        );
        let error = projector()
            .push(stream.as_bytes())
            .expect_err("contradictory terminal");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
        assert!(error.to_string().contains(message));
    }
    let mismatched_details = tool_stream(
        COMPLETE_INPUT,
        "max_tokens",
        Some(serde_json::json!({"type": "client_tool"})),
    );
    assert_eq!(
        projector()
            .push(mismatched_details.as_bytes())
            .expect_err("stop details disagree")
            .kind(),
        MessagesStreamErrorKind::InvalidResponse
    );
}

#[test]
fn output_limit_cannot_revoke_possible_hosted_effects() {
    let server = sse(
        "content_block_start",
        &serde_json::json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "server_tool_use", "id": "srv_1", "name": "code_execution", "input": {}}}),
    ) + &sse(
        "content_block_stop",
        &serde_json::json!({"type": "content_block_stop", "index": 0}),
    );
    let stream = tool_stream(TRUNCATED_INPUT, "max_tokens", None)
        .replace("\"index\":1", "\"index\":2")
        .replace("\"index\":0", "\"index\":1")
        .replacen(
            "event: content_block_start\n",
            &format!("{server}event: content_block_start\n"),
            1,
        );
    let mut projector = projector_with_hosted(true);
    let error = projector
        .push(stream.as_bytes())
        .expect_err("hosted output limit needs reconciliation");
    assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
    assert!(
        error
            .to_string()
            .contains("output limit after possible hosted effects")
    );
    assert!(projector.hosted_effects_possible());
    assert!(projector.into_terminal_projection().is_err());
}
