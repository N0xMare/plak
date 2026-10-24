use super::*;
use serde_json::json;

#[test]
fn xai_policy_rejects_double_create() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::xai());
    let event = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "model": "grok-3",
            "status": "in_progress",
            "output": []
        }
    });
    let object = event.as_object().unwrap();
    life.response_created(object, Some("resp_1"), Some("grok-3"), "grok-3")
        .unwrap();
    let err = life
        .response_created(object, Some("resp_1"), Some("grok-3"), "grok-3")
        .unwrap_err();
    assert_eq!(
        err,
        ResponsesLifecycleError::InvalidLifecycle {
            reason: "response was created more than once"
        }
    );
}

#[test]
fn openrouter_policy_requires_create_identity() {
    assert!(!LifecycleDialectPolicy::openrouter().intermediate_response_id);
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openrouter());
    let event = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "model": "m",
            "status": "in_progress",
            "output": []
        }
    });
    let object = event.as_object().unwrap();
    let err = life
        .response_created(object, None, Some("m"), "m")
        .unwrap_err();
    assert_eq!(
        err,
        ResponsesLifecycleError::InvalidLifecycle {
            reason: "response ID is empty"
        }
    );
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openrouter());
    life.response_created(object, Some("resp_1"), Some("m"), "m")
        .expect("valid OpenRouter create identity");
    let intermediate = json!({
        "type": "response.output_text.delta",
        "item_id": "msg_1",
        "output_index": 0,
        "content_index": 0,
        "delta": "ok"
    });
    life.validate_intermediate_response_id(
        intermediate.as_object().expect("intermediate object"),
        "response.output_text.delta",
    )
    .expect("current OpenRouter intermediate event omits response_id");
}

#[test]
fn openai_policy_requires_create_identity_and_skips_response_id() {
    assert_eq!(
        LifecycleDialectPolicy::openai(),
        LifecycleDialectPolicy {
            require_created_id: true,
            require_created_model: true,
            model_binding: ResponseModelBinding::ProviderReported,
            reasoning: ReasoningLifecycleMode::FullParts,
            content_text_done: true,
            reasoning_content_text: false,
            intermediate_response_id: false,
            strict_terminal_output: false,
            reasoning_terminal_ciphertext_refresh: false,
            sparse_terminal_model_from_prepared: false,
            sparse_terminal_output_from_completed: false,
            incomplete_length_text_snapshot: false,
        }
    );
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai());
    let err = life
        .output_item_added(0, &json!({"type": "message", "id": "m"}))
        .expect_err("openai requires create before items");
    assert!(matches!(
        err,
        ResponsesLifecycleError::InvalidLifecycle { .. }
    ));
    // Platform create often omits status/output.
    let event = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "model": "gpt-test"
        }
    });
    let object = event.as_object().unwrap();
    life.response_created(object, Some("resp_1"), Some("gpt-test"), "gpt-test")
        .expect("create without status/output");
    // Intermediate response_id is not enforced for OpenAI.
    life.validate_intermediate_response_id(object, "response.output_item.added")
        .expect("no intermediate response_id policy");
    life.output_item_added(
        0,
        &json!({"type": "message", "id": "m", "role": "assistant", "content": []}),
    )
    .expect("item after create");
    // Contiguous indices + double create.
    let err = life
        .output_item_added(
            2,
            &json!({"type": "message", "id": "m2", "role": "assistant", "content": []}),
        )
        .expect_err("non-contiguous");
    assert_eq!(
        err,
        ResponsesLifecycleError::InvalidLifecycle {
            reason: "output item indices are not contiguous and ordered"
        }
    );
    let err = life
        .response_created(object, Some("resp_1"), Some("gpt-test"), "gpt-test")
        .expect_err("double create");
    assert_eq!(
        err,
        ResponsesLifecycleError::InvalidLifecycle {
            reason: "response was created more than once"
        }
    );
}

#[test]
fn openai_accepts_seeded_encrypted_reasoning_and_custom_tool() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai());
    let event = json!({
        "type": "response.created",
        "response": { "id": "resp_1", "model": "gpt-test", "status": "in_progress" }
    });
    life.response_created(
        event.as_object().unwrap(),
        Some("resp_1"),
        Some("gpt-test"),
        "gpt-test",
    )
    .unwrap();
    life.output_item_added(
        0,
        &json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "opaque-fixture",
            "summary": []
        }),
    )
    .expect("encrypted seed on add");
    life.output_item_added(
        1,
        &json!({
            "type": "custom_tool_call",
            "id": "custom_one",
            "call_id": "custom_call_one",
            "name": "write_code",
            "input": ""
        }),
    )
    .expect("custom tool add");
    life.function_arguments_delta(1, Some("custom_one"), "print(1)")
        .unwrap();
    life.function_arguments_done(1, Some("custom_one"), "print(1)")
        .unwrap();
    life.output_item_done(
        1,
        &json!({
            "type": "custom_tool_call",
            "id": "custom_one",
            "call_id": "custom_call_one",
            "name": "write_code",
            "input": "print(1)",
            "status": "completed"
        }),
    )
    .expect("custom tool done");
}

#[test]
fn openai_codex_accepts_compaction_items_without_status() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai_codex());
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_compact",
            "model": "gpt-5.3-codex-spark"
        }
    });
    life.response_created(
        created.as_object().unwrap(),
        Some("resp_compact"),
        Some("gpt-5.3-codex-spark"),
        "gpt-5.3-codex-spark",
    )
    .expect("create");
    life.output_item_added(
        0,
        &json!({
            "type": "compaction",
            "id": "cmp_1",
            "encrypted_content": "ciphertext"
        }),
    )
    .expect("compaction add");
    life.output_item_done(
        0,
        &json!({
            "type": "compaction",
            "id": "cmp_1",
            "encrypted_content": "ciphertext"
        }),
    )
    .expect("compaction done without status");
}

#[test]
fn openai_codex_accepts_created_model_distinct_from_requested_catalog_slug() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai_codex());
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_mini",
            "model": "gpt-5.4-mini-2026-03-17",
            "status": "in_progress",
            "output": []
        }
    });
    life.response_created(
        created.as_object().unwrap(),
        Some("resp_mini"),
        Some("gpt-5.4-mini-2026-03-17"),
        "gpt-5.4-mini",
    )
    .expect("Codex OAuth does not ExactRequest SSE response.model");
    assert_eq!(life.created_model(), Some("gpt-5.4-mini-2026-03-17"));
    assert!(
        life.terminal_response(
            "resp_mini",
            Some("other-backend"),
            "completed",
            "completed",
            None,
            false,
            &[],
        )
        .is_err(),
        "later events must still agree with the created reported model"
    );
}

#[test]
fn openai_platform_rejects_created_model_distinct_from_requested() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai_platform_api());
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_plat",
            "model": "gpt-5.4-mini-2026-03-17",
            "status": "in_progress",
            "output": []
        }
    });
    assert_eq!(
        life.response_created(
            created.as_object().unwrap(),
            Some("resp_plat"),
            Some("gpt-5.4-mini-2026-03-17"),
            "gpt-5.4-mini",
        ),
        Err(ResponsesLifecycleError::BindingMismatch)
    );
}

#[test]
fn openai_codex_accepts_opaque_items_without_status() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai_codex());
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_opaque",
            "model": "gpt-5.3-codex-spark"
        }
    });
    life.response_created(
        created.as_object().unwrap(),
        Some("resp_opaque"),
        Some("gpt-5.3-codex-spark"),
        "gpt-5.3-codex-spark",
    )
    .expect("create");
    life.output_item_added(
        0,
        &json!({
            "type": "codex_something",
            "id": "op_1"
        }),
    )
    .expect("opaque add");
    life.output_item_done(
        0,
        &json!({
            "type": "codex_something",
            "id": "op_1"
        }),
    )
    .expect("opaque done without status");
}

#[test]
fn xai_rejects_opaque_items_without_status() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::xai());
    let created = json!({
        "type": "response.created",
        "response": {
            "status": "in_progress",
            "output": []
        }
    });
    life.response_created(created.as_object().unwrap(), None, None, "grok-4.5")
        .expect("create");
    life.output_item_added(
        0,
        &json!({
            "type": "codex_something",
            "id": "op_1"
        }),
    )
    .expect("opaque add");
    let err = life
        .output_item_done(
            0,
            &json!({
                "type": "codex_something",
                "id": "op_1"
            }),
        )
        .expect_err("xAI requires opaque status");
    assert_eq!(
        err,
        ResponsesLifecycleError::InvalidLifecycle {
            reason: "opaque completion status is missing"
        }
    );
}

#[test]
fn exact_binding_mismatch_on_model() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openrouter());
    let event = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "model": "other",
            "status": "in_progress",
            "output": []
        }
    });
    let object = event.as_object().unwrap();
    assert_eq!(
        life.response_created(object, Some("resp_1"), Some("other"), "expected"),
        Err(ResponsesLifecycleError::BindingMismatch)
    );
}

#[test]
fn xai_accepts_omitted_create_identity() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::xai());
    let event = json!({
        "type": "response.created",
        "response": {
            "status": "in_progress",
            "output": []
        }
    });
    life.response_created(event.as_object().unwrap(), None, None, "grok-4.5")
        .expect("xAI create identity is optional");
}

#[test]
fn xai_retains_provider_reported_model_and_requires_terminal_consistency() {
    let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::xai());
    let event = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "model": "routed-backend",
            "status": "in_progress",
            "output": []
        }
    });
    life.response_created(
        event.as_object().unwrap(),
        Some("resp_1"),
        Some("routed-backend"),
        "grok-4.5",
    )
    .expect("xAI preserves provider-reported routed identity");
    assert_eq!(life.created_model(), Some("routed-backend"));
    assert!(
        life.terminal_response(
            "resp_1",
            Some("another-backend"),
            "completed",
            "completed",
            None,
            false,
            &[],
        )
        .is_err()
    );
}

fn reasoning_item(ciphertext: &str) -> Value {
    json!({
        "id": "rs_1",
        "type": "reasoning",
        "status": "completed",
        "summary": [],
        "encrypted_content": ciphertext
    })
}

fn finish_one_item(
    policy: LifecycleDialectPolicy,
    done: &Value,
    terminal: &Value,
) -> Result<(), ResponsesLifecycleError> {
    let mut life = ResponsesLifecycle::new(policy);
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "model": "gpt-test",
            "status": "in_progress",
            "output": []
        }
    });
    life.response_created(
        created.as_object().expect("created object"),
        Some("resp_1"),
        Some("gpt-test"),
        "gpt-test",
    )?;
    let mut added = done.clone();
    if let Some(object) = added.as_object_mut() {
        object.remove("status");
        if object.get("type").and_then(Value::as_str) == Some("reasoning") {
            object.insert("summary".to_owned(), json!([]));
        }
    }
    life.output_item_added(0, &added)?;
    life.output_item_done(0, done)?;
    life.terminal_response(
        "resp_1",
        Some("gpt-test"),
        "completed",
        "completed",
        None,
        false,
        &[terminal],
    )
}

#[test]
fn platform_terminal_accepts_reencrypted_reasoning_only() {
    assert!(LifecycleDialectPolicy::openai_platform_api().reasoning_terminal_ciphertext_refresh);
    assert!(!LifecycleDialectPolicy::openai_codex().reasoning_terminal_ciphertext_refresh);
    assert!(!LifecycleDialectPolicy::xai().reasoning_terminal_ciphertext_refresh);
    assert!(!LifecycleDialectPolicy::grok().reasoning_terminal_ciphertext_refresh);
    assert!(!LifecycleDialectPolicy::openrouter().reasoning_terminal_ciphertext_refresh);

    let done = reasoning_item("synthetic-streamed-ciphertext");
    let mut terminal = reasoning_item("synthetic-terminal-ciphertext");
    finish_one_item(
        LifecycleDialectPolicy::openai_platform_api(),
        &done,
        &terminal,
    )
    .expect("platform keeps a reasoning ciphertext refresh");
    // The lifecycle does not rewrite the terminal item. Replay uses it.
    assert_eq!(
        terminal["encrypted_content"],
        "synthetic-terminal-ciphertext"
    );

    terminal["summary"] = json!([{"type": "summary_text", "text": "changed"}]);
    let err = finish_one_item(
        LifecycleDialectPolicy::openai_platform_api(),
        &done,
        &terminal,
    )
    .expect_err("another reasoning field still fails");
    assert_eq!(
        err,
        ResponsesLifecycleError::InvalidLifecycle {
            reason: "terminal response changed a completed provider item"
        }
    );

    let compaction_done = json!({
        "id": "cmp_1",
        "type": "compaction",
        "encrypted_content": "synthetic-streamed-ciphertext"
    });
    let compaction_terminal = json!({
        "id": "cmp_1",
        "type": "compaction",
        "encrypted_content": "synthetic-terminal-ciphertext"
    });
    let err = finish_one_item(
        LifecycleDialectPolicy::openai_platform_api(),
        &compaction_done,
        &compaction_terminal,
    )
    .expect_err("compaction ciphertext stays exact");
    assert_eq!(
        err,
        ResponsesLifecycleError::InvalidLifecycle {
            reason: "terminal response changed a completed provider item"
        }
    );

    for policy in [
        LifecycleDialectPolicy::xai(),
        LifecycleDialectPolicy::openrouter(),
        LifecycleDialectPolicy::grok(),
    ] {
        let err = finish_one_item(
            policy,
            &reasoning_item("synthetic-streamed-ciphertext"),
            &reasoning_item("synthetic-terminal-ciphertext"),
        )
        .expect_err("only the platform route refreshes reasoning ciphertext");
        assert_eq!(
            err,
            ResponsesLifecycleError::InvalidLifecycle {
                reason: "terminal response changed a completed provider item"
            }
        );
    }
}
