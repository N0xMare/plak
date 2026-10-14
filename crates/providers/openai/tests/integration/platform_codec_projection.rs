//! Credential-free `OpenAI` Platform Responses codec/projector evidence.

use std::error::Error;

use plakous_responses::{LifecycleDialectPolicy, ResponsesDialectProfile, ResponsesStreamLimits};
use plakous_responses_dialect::{
    ResponsesContentPart, ResponsesFamilySseDecoder, ResponsesFamilyStreamError,
    ResponsesOutputItem, ResponsesResponseLimits, ResponsesResponseStatus,
    ResponsesStreamCapabilities, ResponsesStreamDialectData, ResponsesStreamEventKind,
    ResponsesUsagePolicy,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const MODEL: &str = "gpt-5.6-sol";

fn decoder() -> ResponsesFamilySseDecoder {
    ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::new(
            ResponsesDialectProfile::openai_platform(),
            LifecycleDialectPolicy::openai_platform_api(),
            MODEL,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
            ResponsesStreamCapabilities::codex(),
        ),
        ResponsesStreamLimits::default(),
    )
}

#[test]
fn platform_codec_projects_exact_terminal_without_dispatch() -> TestResult {
    let native = serde_json::json!({
        "id": "resp_platform_golden",
        "object": "response",
        "status": "completed",
        "model": MODEL,
        "output": [{
            "id": "msg_platform_golden",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "codec ok"}]
        }],
        "usage": {
            "input_tokens": 11,
            "output_tokens": 3,
            "total_tokens": 14,
            "input_tokens_details": {"cached_tokens": 2},
            "output_tokens_details": {"reasoning_tokens": 1}
        }
    });
    let stream = format!(
        concat!(
            "data: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"resp_platform_golden\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"{}\"}}}}\n\n",
            "data: {{\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{{\"id\":\"msg_platform_golden\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}}}\n\n",
            "data: {{\"type\":\"response.content_part.added\",\"sequence_number\":2,\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"\"}}}}\n\n",
            "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"output_index\":0,\"content_index\":0,\"delta\":\"codec ok\"}}\n\n",
            "data: {{\"type\":\"response.output_text.done\",\"sequence_number\":4,\"output_index\":0,\"content_index\":0,\"text\":\"codec ok\"}}\n\n",
            "data: {{\"type\":\"response.content_part.done\",\"sequence_number\":5,\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"codec ok\"}}}}\n\n",
            "data: {{\"type\":\"response.output_item.done\",\"sequence_number\":6,\"output_index\":0,\"item\":{{\"id\":\"msg_platform_golden\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"codec ok\"}}]}}}}\n\n",
            "data: {{\"type\":\"response.completed\",\"sequence_number\":7,\"response\":{}}}\n\n"
        ),
        MODEL, native
    );
    let mut codec = decoder();
    let events = codec.push(stream.as_bytes())?;
    assert_eq!(
        codec.finish()?,
        [] as [ResponsesStreamEventKind<
            plakous_responses_dialect::ResponsesResponse,
            plakous_responses_dialect::ResponsesProviderError,
        >; 0]
    );
    let response = events
        .iter()
        .find_map(|event| match event {
            ResponsesStreamEventKind::ResponseCompleted(response) => Some(response),
            _ => None,
        })
        .ok_or("OpenAI golden omitted terminal projection")?;

    assert_eq!(response.status(), &ResponsesResponseStatus::Completed);
    assert_eq!(response.model(), MODEL);
    assert_eq!(response.native(), &native);
    assert!(matches!(
        response.output(),
        [ResponsesOutputItem::Message { content, .. }]
            if matches!(content.as_slice(), [ResponsesContentPart::OutputText { text, .. }] if text == "codec ok")
    ));
    let usage = response.usage().ok_or("OpenAI golden omitted usage")?;
    assert_eq!(usage.input_tokens(), Some(11));
    assert_eq!(usage.output_tokens(), Some(3));
    assert_eq!(usage.total_tokens(), Some(14));
    assert_eq!(usage.cached_input_tokens(), Some(2));
    assert_eq!(usage.reasoning_tokens(), Some(1));

    let mut wrong_model = stream.replace(MODEL, "foreign-model");
    wrong_model = wrong_model.replacen("foreign-model", MODEL, 1);
    assert!(decoder().push(wrong_model.as_bytes()).is_err());
    Ok(())
}

#[test]
fn platform_ptc_second_pause_aggregate_and_final_stream_preserve_native_lineage() -> TestResult {
    let aggregate: serde_json::Value =
        serde_json::from_slice(include_bytes!("../fixtures/platform_ptc_second_pause.json"))?;
    let parsed = plakous_responses_dialect::ResponsesResponse::parse(
        &aggregate,
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    )?;
    assert_eq!(parsed.id(), "resp_ptc_second_pause");
    assert!(matches!(
        parsed.output(),
        [ResponsesOutputItem::FunctionCall { native, .. }]
            if native["call_id"] == "call_ptc_second"
                && native["caller"]["type"] == "program"
                && native["caller"]["caller_id"] == "pcall_ptc_root"
    ));
    assert_eq!(parsed.native(), &aggregate);

    let mut codec = decoder();
    let mut events = codec.push(include_bytes!("../fixtures/platform_ptc_final.sse"))?;
    events.extend(codec.finish()?);
    let terminal = events
        .iter()
        .find_map(|event| match event {
            ResponsesStreamEventKind::ResponseCompleted(response) => Some(response),
            _ => None,
        })
        .ok_or("OpenAI PTC fixture omitted final terminal")?;
    assert!(matches!(
        terminal.output(),
        [ResponsesOutputItem::Opaque { kind, native }]
            if kind == "program_output"
                && native["id"] == "po_ptc_final"
                && native["call_id"] == "pcall_ptc_root"
                && native["result"] == "first + second"
                && native["status"] == "completed"
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                ResponsesStreamEventKind::OutputItemAdded { .. }
                    | ResponsesStreamEventKind::OutputItemDone { .. }
            ))
            .count(),
        2
    );
    Ok(())
}

const STREAMED_CIPHERTEXT: &str = "synthetic-streamed-ciphertext";
const TERMINAL_CIPHERTEXT: &str = "synthetic-terminal-ciphertext";

fn sse_event(value: &serde_json::Value) -> String {
    let mut out = format!("data: {value}");
    out.push('\n');
    out.push('\n');
    out
}

fn reasoning_item(ciphertext: &str, summary: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "rs_fixture",
        "type": "reasoning",
        "status": "completed",
        "summary": summary,
        "encrypted_content": ciphertext
    })
}

fn platform_reasoning_stream(terminal_item: &serde_json::Value) -> String {
    let created = serde_json::json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {
            "id": "resp_reencrypt",
            "object": "response",
            "status": "in_progress",
            "model": MODEL
        }
    });
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "sequence_number": 1,
        "output_index": 0,
        "item": {
            "id": "rs_fixture",
            "type": "reasoning",
            "summary": [],
            "encrypted_content": STREAMED_CIPHERTEXT
        }
    });
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "sequence_number": 2,
        "output_index": 0,
        "item": reasoning_item(STREAMED_CIPHERTEXT, &serde_json::json!([]))
    });
    let completed = serde_json::json!({
        "type": "response.completed",
        "sequence_number": 3,
        "response": {
            "id": "resp_reencrypt",
            "object": "response",
            "status": "completed",
            "model": MODEL,
            "output": [terminal_item],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        }
    });
    [created, added, done, completed]
        .iter()
        .map(sse_event)
        .collect()
}

#[test]
fn platform_stream_keeps_terminal_reasoning_ciphertext_when_only_that_field_changes() -> TestResult
{
    let stream =
        platform_reasoning_stream(&reasoning_item(TERMINAL_CIPHERTEXT, &serde_json::json!([])));
    let mut codec = decoder();
    let events = codec.push(stream.as_bytes())?;
    assert_eq!(
        codec.finish()?,
        [] as [ResponsesStreamEventKind<
            plakous_responses_dialect::ResponsesResponse,
            plakous_responses_dialect::ResponsesProviderError,
        >; 0]
    );
    let done = events.iter().find_map(|event| match event {
        ResponsesStreamEventKind::OutputItemDone { item, .. } => Some(item),
        _ => None,
    });
    let done = done.ok_or("fixture omitted output_item.done")?;
    assert_eq!(done.kind(), "reasoning");
    assert_eq!(done.native()["encrypted_content"], STREAMED_CIPHERTEXT);
    let response = events.iter().find_map(|event| match event {
        ResponsesStreamEventKind::ResponseCompleted(response) => Some(response),
        _ => None,
    });
    let response = response.ok_or("fixture omitted response.completed")?;
    let item = response
        .output()
        .first()
        .ok_or("terminal omitted reasoning")?;
    assert_eq!(item.kind(), "reasoning");
    assert_eq!(item.native()["encrypted_content"], TERMINAL_CIPHERTEXT);
    assert_eq!(
        response.native()["output"][0]["encrypted_content"],
        TERMINAL_CIPHERTEXT
    );
    Ok(())
}

#[test]
fn platform_stream_rejects_reasoning_terminal_that_changes_another_field() -> TestResult {
    let mut terminal = reasoning_item(TERMINAL_CIPHERTEXT, &serde_json::json!([]));
    terminal["summary"] = serde_json::json!([{"type": "summary_text", "text": "changed"}]);
    let stream = platform_reasoning_stream(&terminal);
    match decoder().push(stream.as_bytes()) {
        Err(error) => {
            assert_eq!(
                error,
                ResponsesFamilyStreamError::Lifecycle(
                    "terminal response changed a completed provider item"
                )
            );
            Ok(())
        }
        Ok(_) => Err("reasoning field change was accepted".into()),
    }
}
