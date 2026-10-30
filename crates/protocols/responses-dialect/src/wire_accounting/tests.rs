use super::*;
use serde_json::json;

#[test]
fn closed_wire_shapes_are_counted_exactly() -> Result<(), WireAccountingError> {
    let body = json!({
        "model": "model-a",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},
            {"id":"message-1","type":"message","status":"completed","role":"assistant","content":[
                {"type":"output_text","text":"answer","annotations":[],"logprobs":[]},
                {"type":"refusal","refusal":"bounded refusal"}
            ]},
            {"type":"function_call","call_id":"call-1","name":"read","arguments":"{}"},
            {"type":"function_call_output","call_id":"call-1","output":[
                {"type":"input_text","text":"part one"},
                {"type":"input_text","text":"part two"}
            ]},
            {"id":"reasoning-1","type":"reasoning","summary":[
                {"type":"summary_text","text":"checked"}
            ],"encrypted_content":"opaque-reasoning"},
            {"id":"future-1","type":"future_output","status":"completed","payload":{"opaque":true}}
        ],
        "stream": true,
        "tools": [{
            "type":"function",
            "name":"read",
            "description":"Read a value",
            "parameters":{
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"path":{"type":"string"}},
                "required":["path"],
                "additionalProperties":false
            }
        }],
        "tool_choice":"auto",
        "max_output_tokens":64,
        "reasoning":{"effort":"xhigh","summary":"concise"},
        "store":false,
        "include":["reasoning.encrypted_content"]
    });
    let counts = inspect_wire_occurrences(&body)?;
    let expected = [
        (ResponsesWireOccurrence::Message, 2),
        (ResponsesWireOccurrence::InputText, 3),
        (ResponsesWireOccurrence::OutputText, 1),
        (ResponsesWireOccurrence::Refusal, 1),
        (ResponsesWireOccurrence::FunctionCall, 1),
        (ResponsesWireOccurrence::FunctionCallOutput, 1),
        (ResponsesWireOccurrence::FunctionTool, 1),
        (ResponsesWireOccurrence::SchemaNode, 9),
        (ResponsesWireOccurrence::ReasoningReplay, 1),
        (ResponsesWireOccurrence::ReasoningSummary, 1),
        (ResponsesWireOccurrence::OpaqueReplay, 1),
    ];
    for (occurrence, expected) in expected {
        assert_eq!(counts.count(occurrence), expected, "{occurrence:?}");
    }
    assert_eq!(counts.total(), 22);
    Ok(())
}

#[test]
fn replayed_output_text_accepts_only_closed_logprob_container_shapes()
-> Result<(), WireAccountingError> {
    let body = |logprobs| {
        json!({
            "model":"model-a",
            "input":[{
                "id":"message-1",
                "type":"message",
                "status":"completed",
                "role":"assistant",
                "content":[{
                    "type":"output_text",
                    "text":"answer",
                    "annotations":[],
                    "logprobs":logprobs
                }]
            }],
            "stream":true,
            "store":false,
            "include":["reasoning.encrypted_content"]
        })
    };
    for value in [
        Value::Null,
        json!([]),
        json!([{"token":"a","logprob":-0.1}]),
    ] {
        let counts = inspect_wire_occurrences(&body(value))?;
        assert_eq!(counts.count(ResponsesWireOccurrence::OutputText), 1);
    }
    assert_eq!(
        inspect_wire_occurrences(&body(json!("not-an-array"))),
        Err(WireAccountingError::Malformed)
    );
    Ok(())
}

#[test]
fn unknown_and_malformed_shapes_are_rejected() {
    let base = json!({
        "model":"model-a",
        "input":[],
        "stream":true
    });
    let mut cases = Vec::new();
    let mut unknown_top = base.clone();
    unknown_top["unknown"] = json!(true);
    cases.push(unknown_top);
    let mut image = base.clone();
    image["input"] = json!([{"type":"message","role":"assistant","content":[
        {"type":"input_image","image_url":"https://example.test/image"}
    ]}]);
    cases.push(image);
    let mut custom = base.clone();
    custom["input"] = json!([{
        "id":"custom-1","type":"custom_tool_call","status":"completed",
        "call_id":"call-1","name":"custom","input":"x"
    }]);
    cases.push(custom);
    let mut reasoning = base.clone();
    reasoning["input"] = json!([{
        "id":"reasoning-1","type":"reasoning","summary":[]
    }]);
    cases.push(reasoning);
    let mut reasoning_status = base.clone();
    reasoning_status["input"] = json!([{
        "id":"reasoning-1","type":"reasoning","status":"completed","summary":[],
        "encrypted_content":"opaque"
    }]);
    cases.push(reasoning_status);
    let mut opaque = base.clone();
    opaque["input"] = json!([{"type":"future_output","payload":{}}]);
    cases.push(opaque);
    let mut invalid_schema = base;
    invalid_schema["tools"] = json!([{
        "type":"function","name":"read","description":"read","parameters":"object"
    }]);
    invalid_schema["tool_choice"] = json!("auto");
    cases.push(invalid_schema);

    for case in cases {
        assert!(inspect_wire_occurrences(&case).is_err(), "{case}");
    }
}

#[test]
fn every_counter_and_the_aggregate_are_checked_and_bounded() {
    let mut aggregate = ResponsesWireOccurrenceCounts::empty();
    aggregate.total = MAX_TOTAL_OCCURRENCES;
    assert_eq!(
        aggregate.increment(ResponsesWireOccurrence::Message),
        Err(WireAccountingError::ResourceLimit)
    );

    let mut individual = ResponsesWireOccurrenceCounts::empty();
    individual.counts[ResponsesWireOccurrence::Message.index()] = MAX_TOTAL_OCCURRENCES;
    assert_eq!(
        individual.increment(ResponsesWireOccurrence::Message),
        Err(WireAccountingError::ResourceLimit)
    );
}
