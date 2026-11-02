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
    for input in [COMPLETE_INPUT, TRUNCATED_INPUT] {
        for projector in [
            projector(),
            claude_oauth_projector(),
            openrouter_projector(),
        ] {
            let (response, events, stop) =
                project(projector, &tool_stream(input, "max_tokens", None)).expect("length");
            assert_eq!(stop, MessagesStopReason::MaxTokens);
            assert_eq!(response.finish().reason(), FinishReason::Length);
            assert!(
                response
                    .finish()
                    .detail()
                    .expect("finish detail")
                    .starts_with("stop_reason=max_tokens")
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

/// `model_context_window_exceeded` is a context overflow, not an empty Length.
/// The stream fails before a terminal response exists, so the driver can fold.
#[test]
fn context_window_stop_fails_closed_as_context_overflow() {
    for details in [
        None,
        Some(serde_json::json!({"type": "model_context_window_exceeded"})),
    ] {
        for input in [COMPLETE_INPUT, TRUNCATED_INPUT] {
            for mut projector in [
                projector(),
                claude_oauth_projector(),
                openrouter_projector(),
            ] {
                let error = projector
                    .push(
                        tool_stream(input, "model_context_window_exceeded", details.clone())
                            .as_bytes(),
                    )
                    .expect_err("context window fails the stream");
                assert!(error.context_overflow(), "{error}");
                assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidEvent);
                assert!(error.to_string().contains("model_context_window_exceeded"));
                assert!(projector.into_terminal_projection().is_err());
            }
        }
    }
}

#[test]
fn context_window_after_hosted_effect_requires_reconciliation() {
    let server = sse(
        "content_block_start",
        &serde_json::json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "server_tool_use", "id": "srv_1", "name": "code_execution", "input": {}}}),
    ) + &sse(
        "content_block_stop",
        &serde_json::json!({"type": "content_block_stop", "index": 0}),
    );
    let stream = tool_stream(TRUNCATED_INPUT, "model_context_window_exceeded", None)
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
        .expect_err("hosted context window needs reconciliation");
    assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
    assert!(!error.context_overflow());
    assert!(
        error
            .to_string()
            .contains("context window after possible hosted effects")
    );
    assert!(projector.hosted_effects_possible());
    assert!(projector.into_terminal_projection().is_err());
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

fn programmatic_stream(input: &str, reason: &str, container: bool) -> String {
    let caller = r#"{"type":"code_execution","tool_id":"srv_prior"}"#;
    let container = if container {
        r#""container":{"id":"container_1","expires_at":"2026-08-23T12:00:00Z"},"#
    } else {
        ""
    };
    format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_prog\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n",
            "event: content_block_start\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"tool_prog\",\"name\":\"weather\",\"input\":{{}},\"caller\":{caller}}}}}\n\n",
            "event: content_block_delta\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{input}}}}}\n\n",
            "event: content_block_stop\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{{container}\"stop_reason\":\"{reason}\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":8}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        caller = caller,
        input = serde_json::to_string(input).expect("json string"),
        container = container,
        reason = reason,
    )
}

/// A programmatic caller is a hosted effect at block start. `max_tokens`
/// revokes the deferred call, complete or cut, as the reconciliation terminal.
#[test]
fn max_tokens_cuts_a_deferred_programmatic_tool_as_reconciliation() {
    for input in [COMPLETE_INPUT, TRUNCATED_INPUT] {
        let mut projector = programmatic_projector();
        let error = projector
            .push(programmatic_stream(input, "max_tokens", false).as_bytes())
            .expect_err("cut programmatic call");
        assert!(!error.context_overflow());
        assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
        assert!(
            error
                .to_string()
                .contains("output limit after possible hosted effects"),
            "{error}"
        );
        assert!(projector.hosted_effects_possible());
        assert!(projector.into_terminal_projection().is_err());
    }
}

#[test]
fn other_stops_still_reject_cut_programmatic_json() {
    let mut projector = programmatic_projector();
    let error = projector
        .push(programmatic_stream(TRUNCATED_INPUT, "end_turn", true).as_bytes())
        .expect_err("cut json under end_turn");
    assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidEvent);
    assert!(
        error.to_string().contains("not complete valid JSON"),
        "{error}"
    );
}

#[test]
fn context_window_drops_a_deferred_programmatic_tool() {
    let mut projector = programmatic_projector();
    let error = projector
        .push(
            programmatic_stream(TRUNCATED_INPUT, "model_context_window_exceeded", false).as_bytes(),
        )
        .expect_err("context window after a programmatic caller");
    assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
    assert!(!error.context_overflow(), "{error}");
    assert!(
        error
            .to_string()
            .contains("context window after possible hosted effects"),
        "{error}"
    );
    assert!(projector.hosted_effects_possible());
}

#[test]
fn max_tokens_after_a_hosted_effect_does_not_type_the_programmatic_cut() {
    let server = sse(
        "content_block_start",
        &serde_json::json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "server_tool_use", "id": "srv_now", "name": "code_execution", "input": {}}}),
    ) + &sse(
        "content_block_stop",
        &serde_json::json!({"type": "content_block_stop", "index": 0}),
    );
    let stream = programmatic_stream(TRUNCATED_INPUT, "max_tokens", false)
        .replace("\"index\":0", "\"index\":1")
        .replacen(
            "event: content_block_start\n",
            &format!("{server}event: content_block_start\n"),
            1,
        )
        .replace("srv_prior", "srv_now");
    let mut projector = programmatic_projector();
    let error = projector
        .push(stream.as_bytes())
        .expect_err("hosted cut needs reconciliation");
    assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
    assert!(
        error
            .to_string()
            .contains("output limit after possible hosted effects")
    );
    assert!(projector.hosted_effects_possible());
}

#[test]
fn a_complete_programmatic_call_still_flushes_under_tool_use() {
    let mut projector = programmatic_projector();
    let events = projector
        .push(programmatic_stream(COMPLETE_INPUT, "tool_use", true).as_bytes())
        .expect("complete programmatic call");
    assert!(projector.hosted_effects_possible());
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::BlockStarted(started)
            if matches!(started.block(), plakous_model::BlockStart::ToolCall { tool, .. } if tool.as_str() == "weather")
    )));
}

/// The stop reason may arrive on a later `message_delta`. The first one, which
/// carries usage and no reason, must not flush the deferred programmatic call.
#[test]
fn a_message_delta_without_a_stop_reason_does_not_flush_a_programmatic_call() {
    let stream = programmatic_stream(COMPLETE_INPUT, "tool_use", true);
    let (head, tail) = stream.split_once("event: message_delta\n").expect("delta");
    let mut projector = programmatic_projector();
    let prelude = format!(
        "{head}event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{}},\"usage\":{{\"output_tokens\":1}}}}\n\n"
    );
    let events = projector
        .push(prelude.as_bytes())
        .expect("usage delta without a stop reason");
    assert!(
        events.iter().all(|event| !matches!(
            event,
            ModelEvent::BlockStarted(started)
                if matches!(started.block(), plakous_model::BlockStart::ToolCall { .. })
        )),
        "a stop-less delta must not flush the deferred call"
    );
    let events = projector
        .push(format!("event: message_delta\n{tail}").as_bytes())
        .expect("the later delta carries the stop reason");
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::BlockStarted(started)
            if matches!(started.block(), plakous_model::BlockStart::ToolCall { tool, .. } if tool.as_str() == "weather")
    )));
}
