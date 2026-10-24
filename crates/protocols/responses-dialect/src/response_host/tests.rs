use super::*;

#[test]
fn openrouter_numeric_payment_required_is_quota_without_a_synthesized_code() {
    let value = serde_json::json!({"code":402,"message":"fixture billing refusal"});
    let Ok(error) = parse_provider_error_with_policy(
        &value,
        ResponsesResponseLimits::default(),
        ResponsesProviderErrorPolicy::OpenRouter,
    ) else {
        panic!("numeric payment-required evidence must decode");
    };
    assert_eq!(error.class(), ResponsesProviderErrorClass::Quota);
    assert_eq!(error.code(), None);
    assert_eq!(error.native(), &value);
}

#[test]
fn openai_overload_code_is_a_typed_server_failure() {
    let Ok(error) = parse_provider_error_with_policy(
        &serde_json::json!({
            "code": "server_is_overloaded",
            "message": "bounded provider detail"
        }),
        ResponsesResponseLimits::default(),
        ResponsesProviderErrorPolicy::OpenAiCompatible,
    ) else {
        panic!("observed Codex overload terminal must decode");
    };
    assert_eq!(error.class(), ResponsesProviderErrorClass::Server);
    assert_eq!(error.code(), Some("server_is_overloaded"));
    assert_eq!(error.message(), Some("bounded provider detail"));
}

#[test]
fn openrouter_failed_response_prefers_canonical_type_and_retains_native_code() {
    let value = serde_json::json!({
        "id": "resp_openrouter_failed",
        "model": "openai/o4-mini",
        "status": "failed",
        "error": {"code": "server_error", "message": "redacted"},
        "error_type": "authentication"
    });
    let Ok(response) = ResponsesResponse::parse_with_error_policy(
        &value,
        ResponsesUsagePolicy::RequireTotal,
        ResponsesProviderErrorPolicy::OpenRouter,
        ResponsesResponseLimits::default(),
    ) else {
        panic!("documented OpenRouter failed response must decode");
    };
    let Some(error) = response.error() else {
        panic!("failed response must retain error");
    };
    assert_eq!(error.class(), ResponsesProviderErrorClass::Authentication);
    assert_eq!(error.kind(), Some("authentication"));
    assert_eq!(error.code(), Some("server_error"));
    assert_eq!(error.native(), &value["error"]);
}

#[test]
fn openrouter_unknown_canonical_type_does_not_inherit_lossy_native_class() {
    let Ok(error) = parse_provider_error_with_canonical_type(
        &serde_json::json!({"code": "server_error", "message": "redacted"}),
        ResponsesResponseLimits::default(),
        ResponsesProviderErrorPolicy::OpenRouter,
        Some("future_error_type"),
    ) else {
        panic!("bounded future canonical type must be retained");
    };
    assert_eq!(error.class(), ResponsesProviderErrorClass::Unknown);
    assert_eq!(error.kind(), Some("future_error_type"));
    assert_eq!(error.code(), Some("server_error"));
}

#[test]
fn opaque_output_items_may_omit_status() {
    let value = serde_json::json!({
        "id": "opaque_1",
        "type": "codex_something",
        "encrypted_content": "ciphertext"
    });
    let Ok(item) = parse_output_item::<ResponsesHostDecodeError>(&value) else {
        panic!("opaque item without status must parse");
    };
    assert!(matches!(
        item,
        ResponsesOutputItem::Opaque { kind, .. } if kind == "codex_something"
    ));
}

#[test]
fn program_items_and_callers_validate_exact_order_and_shape() {
    let response = serde_json::json!({
        "id":"resp_ptc","object":"response","status":"completed","model":"gpt-5.6-sol",
        "output":[
            {"type":"program","id":"prog_1","call_id":"pcall_1","code":"await lookup({q:'x'})","fingerprint":"opaque-fp"},
            {"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"x\"}","caller":{"type":"program","caller_id":"pcall_1"}},
            {"type":"program_output","id":"po_1","call_id":"pcall_1","result":"done","status":"completed"}
        ]
    });
    let Ok(parsed) = ResponsesResponse::parse(
        &response,
        ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
    ) else {
        panic!("exact program sequence must decode");
    };
    assert_eq!(parsed.output()[0].native()["fingerprint"], "opaque-fp");
    assert_eq!(
        parsed.output()[1].native()["caller"]["caller_id"],
        "pcall_1"
    );

    for malformed in [
        serde_json::json!({"type":"program","id":"p","call_id":"c","code":"x"}),
        serde_json::json!({"type":"program","id":"p","call_id":"c","fingerprint":"fp"}),
        serde_json::json!({"type":"program","id":"p","code":"x","fingerprint":"fp"}),
        serde_json::json!({"type":"program","call_id":"c","code":"x","fingerprint":"fp"}),
        serde_json::json!({"type":"program_output","id":"o","call_id":"c","status":"completed"}),
        serde_json::json!({"type":"program_output","id":"o","result":"x","status":"completed"}),
        serde_json::json!({"type":"program_output","call_id":"c","result":"x","status":"completed"}),
        serde_json::json!({"type":"function_call","id":"f","call_id":"c","name":"lookup","arguments":"{}","caller":{"type":"program"}}),
        serde_json::json!({"type":"function_call","id":"f","call_id":"c","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"c","extra":true}}),
        serde_json::json!({"type":"program_output","id":"o","call_id":"c","result":"x","status":"future"}),
    ] {
        let candidate = serde_json::json!({
            "id":"bad","object":"response","status":"completed","model":"gpt-5.6-sol",
            "output":[malformed]
        });
        assert!(
            ResponsesResponse::parse(
                &candidate,
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .is_err()
        );
    }

    for later in [
        serde_json::json!({"type":"function_call","id":"f","call_id":"c","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"prior"}}),
        serde_json::json!({"type":"program_output","id":"o","call_id":"prior","result":"x","status":"completed"}),
    ] {
        let candidate = serde_json::json!({
            "id":"later","object":"response","status":"completed","model":"gpt-5.6-sol",
            "output":[later]
        });
        assert!(
            ResponsesResponse::parse(
                &candidate,
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .is_ok(),
            "aggregate decoding must defer prior-artifact lineage to the projector"
        );
    }

    let incomplete = serde_json::json!({
        "id":"resp_ptc_incomplete","object":"response","status":"incomplete","model":"gpt-5.6-sol",
        "incomplete_details":{"reason":"max_output_tokens"},
        "output":[
            {"type":"program","id":"p","call_id":"c","code":"x","fingerprint":"fp"},
            {"type":"program_output","id":"o","call_id":"c","result":"partial","status":"incomplete"}
        ]
    });
    assert!(
        ResponsesResponse::parse(
            &incomplete,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .is_ok()
    );
}

#[test]
fn unknown_output_item_is_opaque_and_member_preserving() {
    let native = serde_json::json!({
        "type":"future_provider_item","id":"future_1","future_member":{"nested":true}
    });
    let Ok(item) = parse_output_item::<ResponsesHostDecodeError>(&native) else {
        panic!("bounded unknown output item must decode");
    };
    assert!(
        matches!(&item, ResponsesOutputItem::Opaque { kind, .. } if kind == "future_provider_item")
    );
    assert_eq!(item.native(), &native);
}
