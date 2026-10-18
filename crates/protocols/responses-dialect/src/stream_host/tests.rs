use super::*;
use crate::response_host::{ResponsesProviderErrorClass, ResponsesUsage};

const MODEL: &str = "gpt-5.4";
const MIXED: &str = include_str!("../../tests/fixtures/response_mixed.sse");
const O1_SURFACES: &str = include_str!("../../tests/fixtures/response_o1_surfaces.sse");
const INCOMPLETE: &str = include_str!("../../tests/fixtures/response_incomplete.sse");
const FAILED_RESIDUAL: &str = include_str!("../../tests/fixtures/response_failed.sse");

#[test]
fn stream_identity_covers_model_binding_and_reasoning_text_lifecycle() -> Result<(), &'static str> {
    let base = ResponsesStreamDialectData::codex(MODEL);
    let base_identity = base.execution_identity().ok_or("base identity")?;

    let mut different_binding = base.clone();
    different_binding.lifecycle.model_binding =
        plakous_responses::ResponseModelBinding::ExactRequest;
    assert_ne!(
        base_identity,
        different_binding
            .execution_identity()
            .ok_or("model-binding identity")?
    );

    let mut reasoning_text = base.clone();
    reasoning_text.lifecycle.reasoning_content_text =
        !reasoning_text.lifecycle.reasoning_content_text;
    assert_ne!(
        base_identity,
        reasoning_text
            .execution_identity()
            .ok_or("reasoning-text identity")?
    );

    let mut grok_length = ResponsesStreamDialectData::grok("grok-test");
    let length_identity = grok_length
        .execution_identity()
        .ok_or("Grok length identity")?;
    grok_length.lifecycle.incomplete_length_text_snapshot = false;
    assert_ne!(
        length_identity,
        grok_length
            .execution_identity()
            .ok_or("strict Grok identity")?
    );

    let mut refreshed = base.clone();
    refreshed.lifecycle.reasoning_terminal_ciphertext_refresh = true;
    assert_ne!(
        base_identity,
        refreshed
            .execution_identity()
            .ok_or("reasoning ciphertext identity")?
    );
    Ok(())
}

fn decode_in_chunks(
    source: &str,
    chunk_bytes: usize,
) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::codex(MODEL),
        ResponsesStreamLimits::default(),
    );
    let mut events = Vec::new();
    for chunk in source.as_bytes().chunks(chunk_bytes) {
        events.extend(decoder.push(chunk)?);
    }
    events.extend(decoder.finish()?);
    Ok(events)
}

fn decode_openrouter(
    source: &str,
) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::openrouter(MODEL),
        ResponsesStreamLimits::default(),
    );
    let mut events = decoder.push(source.as_bytes())?;
    events.extend(decoder.finish()?);
    Ok(events)
}

fn grok_incomplete_text_events() -> Vec<Value> {
    vec![
        serde_json::json!({"type":"response.created","sequence_number":0,
            "response":{"id":"resp-grok-length","model":"grok-test"}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":1,
            "output_index":0,"item":{"id":"reason","type":"reasoning","summary":[]}}),
        serde_json::json!({"type":"response.output_item.done","sequence_number":2,
            "output_index":0,"item":{"id":"reason","type":"reasoning","status":"completed","summary":[]}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":3,
            "output_index":1,"item":{"id":"message","type":"message","role":"assistant","content":[]}}),
        serde_json::json!({"type":"response.content_part.added","sequence_number":4,
            "item_id":"message","output_index":1,"content_index":0,
            "part":{"type":"output_text","text":"","annotations":[]}}),
        serde_json::json!({"type":"response.output_text.delta","sequence_number":5,
            "item_id":"message","output_index":1,"content_index":0,"delta":"summary"}),
        serde_json::json!({"type":"response.incomplete","sequence_number":6,
            "response":{"id":"resp-grok-length","object":"response","status":"incomplete",
                "model":"grok-test","output":[
                    {"id":"reason","type":"reasoning","status":"completed","summary":[]},
                    {"id":"message","type":"message","role":"assistant","status":"incomplete",
                        "content":[{"type":"output_text","text":"summary","annotations":[]}]}
                ],"usage":{"input_tokens":2,"output_tokens":7,"total_tokens":9},
                "incomplete_details":{"reason":"max_output_tokens"}}}),
    ]
}

fn decode_grok_events(
    events: &[Value],
) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::grok("grok-test"),
        ResponsesStreamLimits::default(),
    );
    let mut output = Vec::new();
    for event in events {
        let frame = format!("data: {event}\n\n");
        for chunk in frame.as_bytes().chunks(11) {
            output.extend(decoder.push(chunk)?);
        }
    }
    output.extend(decoder.finish()?);
    Ok(output)
}

#[test]
fn grok_length_terminal_accepts_exact_unclosed_text_after_completed_reasoning()
-> Result<(), ResponsesFamilyStreamError> {
    let events = decode_grok_events(&grok_incomplete_text_events())?;
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    let Some(ResponsesStreamEventKind::ResponseIncomplete(response)) = events.last() else {
        panic!("Grok length terminal remains typed incomplete");
    };
    assert_eq!(response.output().len(), 2);
    assert_eq!(
        response.usage().and_then(ResponsesUsage::total_tokens),
        Some(9)
    );
    Ok(())
}

#[test]
fn grok_length_terminal_rejects_mismatched_or_non_length_unclosed_items() {
    for mutate in [
        |events: &mut Vec<Value>| {
            events[6]["response"]["output"][1]["content"][0]["text"] = serde_json::json!("changed");
        },
        |events: &mut Vec<Value>| {
            events[6]["response"]["output"][1]["id"] = serde_json::json!("other");
        },
        |events: &mut Vec<Value>| {
            events[6]["response"]["output"][1]["role"] = serde_json::json!("user");
        },
        |events: &mut Vec<Value>| events[4]["content_index"] = serde_json::json!(1),
        |events: &mut Vec<Value>| events[6]["response"]["incomplete_details"] = Value::Null,
        |events: &mut Vec<Value>| {
            events[6]["response"]["incomplete_details"]["reason"] = serde_json::json!("other");
        },
        |events: &mut Vec<Value>| {
            events[6]["type"] = serde_json::json!("response.completed");
            events[6]["response"]["status"] = serde_json::json!("completed");
            events[6]["response"]["incomplete_details"] = Value::Null;
        },
        |events: &mut Vec<Value>| {
            events[3]["item"]["type"] = serde_json::json!("function_call");
        },
    ] {
        let mut events = grok_incomplete_text_events();
        mutate(&mut events);
        assert!(decode_grok_events(&events).is_err());
    }
}

#[test]
fn grok_length_terminal_never_finishes_an_unclosed_function_call() {
    let events = vec![
        serde_json::json!({"type":"response.created","sequence_number":0,
            "response":{"id":"resp-call","model":"grok-test"}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":1,
            "output_index":0,"item":{"id":"call-item","type":"function_call",
                "call_id":"call-1","name":"lookup","arguments":""}}),
        serde_json::json!({"type":"response.function_call_arguments.delta","sequence_number":2,
            "item_id":"call-item","output_index":0,"delta":"{\"q\":1}"}),
        serde_json::json!({"type":"response.incomplete","sequence_number":3,
            "response":{"id":"resp-call","object":"response","status":"incomplete",
                "model":"grok-test","output":[{"id":"call-item","type":"function_call",
                    "status":"incomplete","call_id":"call-1","name":"lookup",
                    "arguments":"{\"q\":1}"}],
                "usage":{"input_tokens":2,"output_tokens":7,"total_tokens":9},
                "incomplete_details":{"reason":"max_output_tokens"}}}),
    ];
    assert!(matches!(
        decode_grok_events(&events),
        Err(ResponsesFamilyStreamError::OutputBoundReached)
    ));
}

#[test]
fn current_openrouter_item_correlated_lifecycle_maps_to_family_events() {
    let stream = format!(
        concat!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-or\",\"model\":\"{}\"}}}}\n\n",
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"id\":\"msg-or\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}}}\n\n",
            "data: {{\"type\":\"response.content_part.added\",\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"\",\"annotations\":[]}}}}\n\n",
            "data: {{\"type\":\"response.content_part.delta\",\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"delta\":\"answer\"}}\n\n",
            "data: {{\"type\":\"response.output_text.done\",\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"text\":\"answer\"}}\n\n",
            "data: {{\"type\":\"response.content_part.done\",\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"answer\",\"annotations\":[]}}}}\n\n",
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"msg-or\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{{\"type\":\"output_text\",\"text\":\"answer\",\"annotations\":[]}}]}}}}\n\n",
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{{\"id\":\"rs-or\",\"type\":\"reasoning\",\"summary\":[]}}}}\n\n",
            "data: {{\"type\":\"response.reasoning_summary_part.added\",\"item_id\":\"rs-or\",\"output_index\":1,\"summary_index\":0,\"part\":{{\"type\":\"summary_text\",\"text\":\"\"}}}}\n\n",
            "data: {{\"type\":\"response.reasoning.delta\",\"item_id\":\"rs-or\",\"output_index\":1,\"delta\":\"think\"}}\n\n",
            "data: {{\"type\":\"response.reasoning_summary_text.done\",\"item_id\":\"rs-or\",\"output_index\":1,\"summary_index\":0,\"text\":\"think\"}}\n\n",
            "data: {{\"type\":\"response.reasoning_summary_part.done\",\"item_id\":\"rs-or\",\"output_index\":1,\"summary_index\":0,\"part\":{{\"type\":\"summary_text\",\"text\":\"think\"}}}}\n\n",
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{{\"id\":\"rs-or\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[{{\"type\":\"summary_text\",\"text\":\"think\"}}]}}}}\n\n",
            "data: {{\"type\":\"response.done\",\"response\":{{\"id\":\"resp-or\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"{}\",\"output\":[{{\"id\":\"msg-or\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{{\"type\":\"output_text\",\"text\":\"answer\",\"annotations\":[]}}]}},{{\"id\":\"rs-or\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[{{\"type\":\"summary_text\",\"text\":\"think\"}}]}}],\"usage\":{{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}}}}\n\n",
            "data: [DONE]\n\n"
        ),
        MODEL, MODEL
    );
    let events = decode_openrouter(&stream)
        .unwrap_or_else(|error| panic!("documented OpenRouter deltas must decode: {error}"));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponsesStreamEventKind::OutputTextDelta { delta, .. } if delta == "answer"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponsesStreamEventKind::ReasoningSummaryTextDelta { delta, .. } if delta == "think"
    )));

    let substituted = stream.replacen(
        "\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"delta\"",
        "\"item_id\":\"rs-or\",\"output_index\":0,\"content_index\":0,\"delta\"",
        1,
    );
    assert!(matches!(
        decode_openrouter(&substituted),
        Err(ResponsesFamilyStreamError::Lifecycle(_))
    ));

    let missing = stream.replacen("\"item_id\":\"msg-or\",", "", 1);
    assert!(matches!(
        decode_openrouter(&missing),
        Err(ResponsesFamilyStreamError::InvalidField("item_id"))
    ));
}

#[test]
fn openrouter_response_error_and_plain_error_use_top_level_canonical_type() {
    for (event_type, canonical, expected) in [
        (
            "response.error",
            "authentication",
            ResponsesProviderErrorClass::Authentication,
        ),
        (
            "error",
            "rate_limit_exceeded",
            ResponsesProviderErrorClass::RateLimited,
        ),
    ] {
        let stream = format!(
            concat!(
                "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-error\",\"model\":\"{}\"}}}}\n\n",
                "data: {{\"type\":\"{}\",\"error\":{{\"code\":\"server_error\",\"message\":\"redacted\"}},\"error_type\":\"{}\"}}\n\n",
                "data: [DONE]\n\n"
            ),
            MODEL, event_type, canonical
        );
        let Ok(events) = decode_openrouter(&stream) else {
            panic!("OpenRouter error event must decode");
        };
        let Some(ResponsesStreamEventKind::Error(error)) = events.last() else {
            panic!("OpenRouter stream must retain the typed provider error");
        };
        assert_eq!(error.class(), expected);
        assert_eq!(error.kind(), Some(canonical));
        assert_eq!(error.code(), Some("server_error"));
    }
}

#[test]
fn openrouter_alias_events_do_not_broaden_codex_typing() {
    let mut hooks = ResponsesFamilyHooks::new(ResponsesStreamDialectData::codex(MODEL));
    let created = serde_json::json!({
        "type": "response.created",
        "response": {"id": "resp-codex", "model": MODEL}
    });
    assert!(hooks.parse_event(&created).is_ok());
    for kind in [
        "response.content_part.delta",
        "response.reasoning.delta",
        "response.error",
    ] {
        let value = serde_json::json!({"type": kind, "delta": "not typed"});
        assert!(matches!(
            hooks.parse_event(&value),
            Ok(ResponsesStreamEventKind::Unknown { kind: retained, .. })
                if retained == kind
        ));
    }
}

#[test]
fn every_byte_split_preserves_text_reasoning_tools_unknown_usage_and_cache() {
    let Ok(events) = decode_in_chunks(MIXED, 1) else {
        panic!("mixed fixture must decode at every byte boundary");
    };
    assert!(events.iter().any(|event| matches!(
        event,
        ResponsesStreamEventKind::OutputTextDelta { delta, .. } if delta.contains('🌍')
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponsesStreamEventKind::ReasoningSummaryTextDelta { delta, .. } if delta == "Checked."
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponsesStreamEventKind::FunctionCallArgumentsDone { arguments, .. }
            if arguments == "{\"q\":\"rust\"}"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponsesStreamEventKind::Unknown { kind, .. }
            if kind == "response.specialized_future_event"
    )));
    let Some(ResponsesStreamEventKind::ResponseCompleted(response)) = events.last() else {
        panic!("mixed fixture must end in a completed response");
    };
    let Some(usage) = response.usage() else {
        panic!("mixed fixture must retain usage");
    };
    assert_eq!(usage.total_tokens(), Some(20));
    assert_eq!(usage.cached_input_tokens(), Some(4));
    assert_eq!(usage.reasoning_tokens(), Some(3));
}

#[test]
fn codex_custom_tools_and_url_citations_are_typed() {
    let Ok(events) = decode_in_chunks(O1_SURFACES, 7) else {
        panic!("Codex surface fixture must decode");
    };
    assert!(events.iter().any(|event| matches!(
        event,
        ResponsesStreamEventKind::OutputTextAnnotationAdded {
            annotation: ResponsesAnnotation::UrlCitation { url, .. },
            ..
        } if url == "https://example.test/docs"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponsesStreamEventKind::CustomToolCallInputDelta { delta, .. }
            if delta == "print(1)"
    )));
    let Some(ResponsesStreamEventKind::ResponseCompleted(response)) = events.last() else {
        panic!("Codex fixture must end in a completed response");
    };
    assert!(matches!(
        response.output().last(),
        Some(ResponsesOutputItem::CustomToolCall { input, .. }) if input == "print(1)"
    ));
}

#[test]
fn codex_soft_terminal_uses_completed_stream_items_when_terminal_output_is_empty() {
    let stream = format!(
        concat!(
            "data: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"resp-soft\",\"model\":\"{}\"}}}}\n\n",
            "data: {{\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{{\"id\":\"msg-soft\",\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[]}}}}\n\n",
            "data: {{\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg-soft\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"\",\"annotations\":[]}}}}\n\n",
            "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg-soft\",\"output_index\":0,\"content_index\":0,\"delta\":\"ok\"}}\n\n",
            "data: {{\"type\":\"response.output_text.done\",\"sequence_number\":4,\"item_id\":\"msg-soft\",\"output_index\":0,\"content_index\":0,\"text\":\"ok\"}}\n\n",
            "data: {{\"type\":\"response.content_part.done\",\"sequence_number\":5,\"item_id\":\"msg-soft\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}}}}\n\n",
            "data: {{\"type\":\"response.output_item.done\",\"sequence_number\":6,\"output_index\":0,\"item\":{{\"id\":\"msg-soft\",\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"status\":\"completed\",\"content\":[{{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}}]}}}}\n\n",
            "data: {{\"type\":\"response.completed\",\"sequence_number\":7,\"response\":{{\"id\":\"resp-soft\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"{}\",\"output\":[],\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\n"
        ),
        MODEL, MODEL
    );
    let Ok(events) = decode_in_chunks(&stream, 11) else {
        panic!("soft Codex terminal must decode");
    };
    let Some(ResponsesStreamEventKind::ResponseCompleted(response)) = events.last() else {
        panic!("soft Codex fixture must end in completion");
    };
    assert_eq!(response.output().len(), 1);
    assert!(matches!(
        response.output().first(),
        Some(ResponsesOutputItem::Message { .. })
    ));
    assert_eq!(response.native()["output"], serde_json::json!([]));
}

#[test]
fn incomplete_and_failed_terminals_remain_typed() {
    let Ok(incomplete) = decode_in_chunks(INCOMPLETE, 13) else {
        panic!("incomplete fixture must decode");
    };
    let Some(ResponsesStreamEventKind::ResponseIncomplete(response)) = incomplete.last() else {
        panic!("incomplete fixture must retain its terminal");
    };
    assert_eq!(
        response
            .incomplete_details()
            .and_then(|details| details.reason()),
        Some("max_output_tokens")
    );

    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::codex(MODEL),
        ResponsesStreamLimits::default(),
    );
    let Ok(before_finish) = decoder.push(FAILED_RESIDUAL.as_bytes()) else {
        panic!("failed residual prefix must decode");
    };
    assert!(
        !before_finish
            .iter()
            .any(ResponsesStreamEventKind::is_terminal)
    );
    let Ok(residual) = decoder.finish() else {
        panic!("EOF must dispatch the residual failed terminal");
    };
    let [ResponsesStreamEventKind::ResponseFailed(response)] = residual.as_slice() else {
        panic!("finish must deliver exactly the residual failed terminal");
    };
    assert_eq!(
        response.error().map(ResponsesProviderError::class),
        Some(ResponsesProviderErrorClass::RateLimited)
    );
}

#[test]
fn malformed_and_limit_failures_poison_without_cloning_state() {
    let mut malformed = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::codex(MODEL),
        ResponsesStreamLimits::default(),
    );
    assert_eq!(
        malformed.push(b"data: {not-json}\n\n"),
        Err(ResponsesFamilyStreamError::InvalidJson)
    );
    assert_eq!(
        malformed.push(b"data: {}\n\n"),
        Err(ResponsesFamilyStreamError::DecoderFailed)
    );

    let limits = ResponsesStreamLimits {
        max_frame_bytes: 32,
        max_total_bytes: 128,
        max_events: 2,
    };
    let mut bounded =
        ResponsesFamilySseDecoder::new(ResponsesStreamDialectData::codex(MODEL), limits);
    assert_eq!(
        bounded.push(b"data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n"),
        Err(ResponsesFamilyStreamError::ResourceLimit("SSE frame bytes"))
    );
    assert_eq!(
        bounded.finish(),
        Err(ResponsesFamilyStreamError::DecoderFailed)
    );

    let Ok(response_limits) = ResponsesResponseLimits::new(1_024, 8, 32) else {
        panic!("valid bounded-unknown test limits");
    };
    let mut unknown = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::codex(MODEL).with_response_limits(response_limits),
        ResponsesStreamLimits::default(),
    );
    let created = format!(
        "data: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"r\",\"model\":\"{MODEL}\"}}}}\n\n"
    );
    let unknown_event = format!(
        "data: {{\"type\":\"response.future\",\"sequence_number\":1,\"future\":\"{}\"}}\n\n",
        "x".repeat(64)
    );
    assert!(unknown.push(created.as_bytes()).is_ok());
    assert_eq!(
        unknown.push(unknown_event.as_bytes()),
        Err(ResponsesFamilyStreamError::ResourceLimit(
            "unknown event bytes"
        ))
    );
}

#[test]
fn grok_allowlist_preserves_foreign_events_and_requires_exact_usage() {
    let stream = concat!(
        "data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{\"id\":\"resp-grok\",\"model\":\"grok-test\"}}\n\n",
        "data: {\"type\":\"response.output_text.done\",\"sequence_number\":2,\"output_index\":0,\"content_index\":0,\"text\":\"future\"}\n\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":3,\"response\":{\"id\":\"resp-grok\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-test\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"total_tokens\":5},\"incomplete_details\":null,\"error\":null}}\n\n"
    );
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::grok("grok-test"),
        ResponsesStreamLimits::default(),
    );
    let mut events = decoder
        .push(stream.as_bytes())
        .unwrap_or_else(|error| panic!("Grok stream must decode: {error:?}"));
    events.extend(
        decoder
            .finish()
            .unwrap_or_else(|error| panic!("Grok stream must finish: {error:?}")),
    );
    assert!(matches!(
        &events[1],
        ResponsesStreamEventKind::Unknown { kind, native }
            if kind == "response.output_text.done"
                && native["text"] == "future"
    ));
    let Some(ResponsesStreamEventKind::ResponseCompleted(response)) = events.last() else {
        panic!("typed Grok terminal");
    };
    assert_eq!(
        response.usage().and_then(ResponsesUsage::total_tokens),
        Some(5)
    );

    let missing_total = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-grok\",\"model\":\"grok-test\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-grok\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-test\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3},\"incomplete_details\":null,\"error\":null}}\n\n"
    );
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::grok("grok-test"),
        ResponsesStreamLimits::default(),
    );
    assert_eq!(
        decoder.push(missing_total.as_bytes()),
        Err(ResponsesFamilyStreamError::InvalidField("total_tokens"))
    );
}

#[test]
fn grok_completed_terminal_ignores_trailing_events_and_bytes() {
    let completed = concat!(
        "data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{\"id\":\"resp-grok\",\"model\":\"grok-test\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp-grok\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-test\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"total_tokens\":5},\"incomplete_details\":null,\"error\":null}}\n\n"
    );
    let trailers: [&[u8]; 4] = [
        b"data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"late\"}\n\n",
        b"data: [DONE]\n\n",
        b"\xff\xfe not json\n\n",
        b"data: {\"type\":\"response.comp",
    ];
    for trailer in trailers {
        for split in [false, true] {
            let mut decoder = ResponsesFamilySseDecoder::new(
                ResponsesStreamDialectData::grok("grok-test"),
                ResponsesStreamLimits::default(),
            );
            let mut events = Vec::new();
            if split {
                events.extend(
                    decoder
                        .push(completed.as_bytes())
                        .unwrap_or_else(|error| panic!("Grok stream must decode: {error:?}")),
                );
                events.extend(decoder.push(trailer).unwrap_or_else(|error| {
                    panic!("trailer after the terminal must be ignored: {error:?}")
                }));
            } else {
                let mut body = completed.as_bytes().to_vec();
                body.extend_from_slice(trailer);
                events.extend(
                    decoder.push(&body).unwrap_or_else(|error| {
                        panic!("buffered trailer must be ignored: {error:?}")
                    }),
                );
            }
            events.extend(
                decoder
                    .finish()
                    .unwrap_or_else(|error| panic!("Grok stream must finish: {error:?}")),
            );
            assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
            assert!(matches!(
                events.last(),
                Some(ResponsesStreamEventKind::ResponseCompleted(_))
            ));
        }
    }
}

#[test]
fn grok_error_allowlist_and_unique_json_are_strict() {
    let error = "data: {\"type\":\"error\",\"error\":{\"type\":\"server_error\",\"code\":\"internal_error\"}}\n\n";
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::grok("grok-test"),
        ResponsesStreamLimits::default(),
    );
    let events = decoder
        .push(error.as_bytes())
        .unwrap_or_else(|error| panic!("Grok error event must decode: {error:?}"));
    assert!(matches!(
        events.as_slice(),
        [ResponsesStreamEventKind::Error(error)]
            if error.class() == ResponsesProviderErrorClass::Server
    ));
    decoder
        .finish()
        .unwrap_or_else(|error| panic!("typed error terminal must finish: {error:?}"));

    let duplicate = "data: {\"type\":\"error\",\"error\":{\"code\":\"invalid_api_key\",\"code\":\"server_error\"}}\n\n";
    let mut decoder = ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::grok("grok-test"),
        ResponsesStreamLimits::default(),
    );
    assert_eq!(
        decoder.push(duplicate.as_bytes()),
        Err(ResponsesFamilyStreamError::InvalidJson)
    );
}
