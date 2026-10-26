use std::sync::Arc;

use plakous_model::{
    ContentBlock, Digest, DigestDomain, EffectiveProviderConfigurationId, FinishReason, ModelEvent,
    ModelId, NamespaceId, ProtocolId, SelectedProviderConfigurationId, SystemClock,
    SystemIdGenerator, ToolName,
};

use super::{
    MessagesStreamDialectHooks, MessagesStreamExtensions, MessagesStreamProjector,
    PreparedMessagesStreamBinding, ToolCaller, parse_tool_caller,
};
use crate::request::ToolOwnership;
use crate::{MessagesRequestPolicy, MessagesStreamErrorKind};

mod native;

fn projector_with_hosted(hosted: bool) -> MessagesStreamProjector {
    let selected: SelectedProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::SelectedProviderConfiguration,
                5,
                &[b"messages-projector-selected"],
            )
            .expect("selected digest"),
        )
        .expect("selected json"),
    )
    .expect("selected");
    let effective: EffectiveProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"messages-projector-effective"],
            )
            .expect("effective digest"),
        )
        .expect("effective json"),
    )
    .expect("effective");
    let digest = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"messages-projector-test"])
        .expect("digest");
    let mut policy = MessagesRequestPolicy::new(
        ProtocolId::new("anthropic.messages").expect("protocol"),
        ModelId::new("claude-sonnet-5").expect("model"),
        None,
        digest,
        digest,
        digest,
        selected,
        effective,
        128,
        None,
        "2023-06-01",
        "oauth-2025-04-20,claude-code-20250219",
    );
    if hosted {
        policy = policy.with_programmatic_tool_calling(
            vec![
                "code_execution_20260120".to_owned(),
                "code_execution_20260521".to_owned(),
            ],
            "code_execution_20260120",
            NamespaceId::new("anthropic.programmatic_tool").expect("namespace"),
        );
    }
    let prepared = PreparedMessagesStreamBinding::new(
        policy,
        digest,
        ToolOwnership::default(),
        MessagesStreamDialectHooks::new(
            "code_execution",
            NamespaceId::new("anthropic.programmatic_tool").expect("namespace"),
        ),
    );
    MessagesStreamProjector::new(prepared, Arc::new(SystemClock), Arc::new(SystemIdGenerator))
        .expect("projector")
}

fn projector() -> MessagesStreamProjector {
    projector_with_hosted(false)
}

fn openrouter_projector() -> MessagesStreamProjector {
    let mut projector = projector();
    projector.prepared.dialect = MessagesStreamDialectHooks::client_tools_only()
        .with_extensions(MessagesStreamExtensions::OpenRouterV1);
    projector
}

fn claude_oauth_projector() -> MessagesStreamProjector {
    let mut projector = projector();
    projector.prepared.dialect = MessagesStreamDialectHooks::client_tools_only()
        .with_extensions(MessagesStreamExtensions::ClaudeOauthV1);
    projector
}

const OPENROUTER_STREAM: &str = include_str!("../../tests/fixtures/stream_openrouter_v1.sse");
const OPENROUTER_INTERLEAVED_TOOL_STREAM: &str =
    include_str!("../../tests/fixtures/stream_openrouter_interleaved_tool.sse");

const CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_context_management\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"CLAUDE_FIXED_OK\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1},\"context_management\":{\"applied_edits\":[]}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn refusal_stream(block: Option<&str>, reason: &str, details: Option<&str>) -> String {
    use std::fmt::Write as _;
    let start = message_start_stream("");
    let mut stream = format!("{}\n\n", start.split("\n\n").next().expect("start"));
    if let Some(block) = block {
        write!(stream,
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{block}}}\n\n\
             event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n"
        ).expect("write fixture");
    }
    let details = details.map_or_else(String::new, |value| format!(",\"stop_details\":{value}"));
    write!(stream,
        "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\",\"stop_sequence\":null{details}}},\"usage\":{{\"output_tokens\":1}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    ).expect("write fixture");
    stream
}

#[test]
fn documented_refusal_shapes_discard_provisional_content_and_preserve_terminal() {
    let details =
        r#"{"type":"refusal","category":"synthetic-policy","explanation":"synthetic explanation"}"#;
    for block in [
        None,
        Some(r#"{"type":"text","text":"partial answer"}"#),
        Some(r#"{"type":"refusal","refusal":"cannot comply"}"#),
        Some(
            r#"{"type":"thinking","thinking":"provisional reasoning","signature":"synthetic-signature"}"#,
        ),
        Some(r#"{"type":"redacted_thinking","data":"synthetic-redacted"}"#),
        Some(
            r#"{"type":"tool_use","id":"refused-call","name":"read_file","input":{"path":"synthetic.txt"}}"#,
        ),
    ] {
        for details in [
            None,
            Some(details),
            Some(r#"{"type":"refusal","category":null,"explanation":null}"#),
        ] {
            for mut projector in [
                projector(),
                claude_oauth_projector(),
                openrouter_projector(),
            ] {
                let mut stream = refusal_stream(block, "refusal", details);
                if projector.prepared.dialect.extensions() == MessagesStreamExtensions::OpenRouterV1
                {
                    stream.push_str("event: data\ndata: [DONE]\n\n");
                }
                let mut assembler =
                    plakous_model::ModelResponseAssembler::new(projector.prepared.plan_digest())
                        .expect("assembler");
                let mut response = None;
                for chunk in stream.as_bytes().chunks(7) {
                    for event in projector.push(chunk).expect("documented refusal SSE") {
                        if let Some(terminal) =
                            assembler.push(event).expect("canonical refusal events")
                        {
                            response = Some(terminal);
                        }
                    }
                }
                projector.finish_eof().expect("terminal EOF");
                let (terminal, _, stop) = projector
                    .into_terminal_projection()
                    .expect("refusal terminal")
                    .into_parts();
                assert_eq!(response.as_ref(), Some(&terminal));
                assert_eq!(terminal.finish().reason(), FinishReason::Refusal);
                assert_eq!(terminal.output(), []);
                assert!(terminal.usage().is_some());
                assert!(terminal.response_id().is_some());
                assert_eq!(
                    stop.details()
                        .map(
                            |value| serde_json::from_str::<serde_json::Value>(value.raw().get())
                                .expect("details JSON")
                        ),
                    details.map(|value| serde_json::from_str::<serde_json::Value>(value)
                        .expect("fixture details"))
                );
            }
        }
    }
}

#[test]
fn closed_client_input_is_validated_only_after_non_refusal_terminal() {
    let block = r#"{"type":"tool_use","id":"provisional-call","name":"read_file","input":{}}"#;
    for input in [
        r#"{"path":"#,
        r#"{"path":"unfinished"#,
        "not-json",
        "[1]",
        "{]",
    ] {
        for reason in ["refusal", "tool_use"] {
            let delta = serde_json::json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": input},
            });
            let stream = refusal_stream(Some(block), reason, None).replace(
                "event: content_block_stop\n",
                &format!(
                    "event: content_block_delta\ndata: {delta}\n\nevent: content_block_stop\n"
                ),
            );
            let stream = stream.replace("\"index\":0", "\"index\":1").replacen(
                "event: content_block_start\n",
                concat!(
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"partial response\"}}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                    "event: content_block_start\n",
                ), 1,
            );
            let mut projector = projector();
            let mut assembler =
                plakous_model::ModelResponseAssembler::new(projector.prepared.plan_digest())
                    .expect("assembler");
            let mut terminal = None;
            let mut rejected = false;
            for chunk in stream.as_bytes().chunks(7) {
                let Ok(events) = projector.push(chunk) else {
                    rejected = true;
                    break;
                };
                for event in events {
                    if let Some(response) =
                        assembler.push(event).expect("revoked provisional events")
                    {
                        terminal = Some(response);
                    }
                }
            }
            if reason == "tool_use" {
                assert!(rejected, "invalid tool input must not dispatch");
                continue;
            }
            assert!(!rejected, "revoked input needs no JSON interpretation");
            projector.finish_eof().expect("closed refusal EOF");
            assert_eq!(terminal.expect("refusal terminal").output(), []);
        }
    }
    let stream = refusal_stream(Some(block), "refusal", None).replace(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "",
    );
    assert!(
        projector().push(stream.as_bytes()).is_err(),
        "wire closure remains required"
    );
}

#[test]
fn refusal_details_are_optional_bounded_and_sanitized() {
    for details in [
        None,
        Some(r#"{"type":"refusal"}"#),
        Some(r#"{"type":"refusal","category":null,"explanation":null}"#),
    ] {
        let mut projector = projector();
        projector
            .push(refusal_stream(None, "refusal", details).as_bytes())
            .expect("optional details");
        projector.finish_eof().expect("EOF");
        let (response, _, _) = projector
            .into_terminal_projection()
            .expect("terminal")
            .into_parts();
        let detail = response.finish().detail().expect("terminal reason");
        assert!(!detail.contains("category="));
        assert!(!detail.contains("explanation="));
    }
    let raw = serde_json::json!({
        "type": "refusal", "category": "synthetic-policy",
        "explanation": format!("\u{1b}[2J\nBearer sk-private-secret {}", "x".repeat(4096)),
    })
    .to_string();
    let mut projector = projector();
    projector
        .push(refusal_stream(None, "refusal", Some(&raw)).as_bytes())
        .expect("diagnostic");
    projector.finish_eof().expect("EOF");
    let (response, _, _) = projector
        .into_terminal_projection()
        .expect("terminal")
        .into_parts();
    let detail = response.finish().detail().expect("diagnostic");
    assert!(detail.contains("category=synthetic-policy"));
    assert!(detail.contains("explanation="));
    assert!(!detail.contains("sk-private-secret"));
    assert!(!detail.chars().any(char::is_control));
    assert!(detail.len() <= plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES);
}

#[test]
fn refusal_cannot_discard_possible_hosted_effects() {
    let stream = refusal_stream(
        Some(r#"{"type":"server_tool_use","id":"server-1","name":"code_execution","input":{}}"#),
        "refusal",
        None,
    );
    let mut projector = projector_with_hosted(true);
    let error = projector
        .push(stream.as_bytes())
        .expect_err("hosted refusal needs reconciliation");
    assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
    assert!(projector.hosted_effects_possible());
    assert!(projector.into_terminal_projection().is_err());
}

#[test]
fn terminal_tool_mismatch_reports_only_fixed_shape() {
    for (block, reason, counts) in [
        (
            Some(
                r#"{"type":"tool_use","id":"private-id","name":"private-tool","input":{"secret":"private-argument"}}"#,
            ),
            "end_turn",
            "client_proposals=1 provider_proposals=0",
        ),
        (None, "tool_use", "client_proposals=0 provider_proposals=0"),
    ] {
        let error = projector()
            .push(refusal_stream(block, reason, None).as_bytes())
            .expect_err("unsupported terminal proposal shape");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
        let message = error.to_string();
        assert!(message.contains(&format!("stop_reason={reason}")));
        assert!(message.contains(counts));
        assert!(!message.contains("private"));
    }
}

#[test]
fn refusal_does_not_relax_terminal_contradictions_or_tool_guards() {
    for stream in [
        refusal_stream(
            Some(r#"{"type":"refusal","refusal":"cannot comply"}"#),
            "end_turn",
            None,
        ),
        refusal_stream(None, "refusal", Some(r#"{"type":"client_tool"}"#)),
        refusal_stream(None, "end_turn", Some(r#"{"type":"refusal"}"#)),
    ] {
        assert_eq!(
            projector()
                .push(stream.as_bytes())
                .expect_err("contradictory terminal")
                .kind(),
            MessagesStreamErrorKind::InvalidResponse
        );
    }
    let fallback = refusal_stream(None, "refusal", Some(r#"{"type":"fallback"}"#));
    assert_eq!(
        projector()
            .push(fallback.as_bytes())
            .expect_err("no automatic fallback")
            .kind(),
        MessagesStreamErrorKind::UnsupportedSemantic
    );
}

#[test]
fn claude_oauth_accepts_bounded_context_management_without_weakening_strict_messages() {
    let mut oauth = claude_oauth_projector();
    let events = oauth
        .push(CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM.as_bytes())
        .expect("pinned Claude OAuth context management delta");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
    );
    oauth.finish_eof().expect("complete Claude OAuth EOF");
    oauth
        .into_terminal_projection()
        .expect("Claude OAuth terminal projection");

    let error = projector()
        .push(CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM.as_bytes())
        .expect_err("strict Messages must reject the OAuth-only field");
    assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);

    let mut unknown = claude_oauth_projector();
    let stream = CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM.replacen(
        "{\"applied_edits\":[]}",
        "{\"future_edits\":[]}",
        1,
    );
    assert_eq!(
        unknown
            .push(stream.as_bytes())
            .expect_err("unknown context management shape")
            .kind(),
        MessagesStreamErrorKind::InvalidResponse
    );
}

fn message_start_stream(message_extra: &str) -> String {
    format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_container\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null{extra},\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n",
            "event: content_block_start\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
            "event: content_block_delta\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"READY\"}}}}\n\n",
            "event: content_block_stop\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        extra = message_extra
    )
}

#[test]
fn message_start_admits_the_container_receipt_without_widening_the_message_object() {
    // The Messages API reports the code-execution container on every
    // `message_start`; it is null whenever the turn allocated none.
    for extra in ["", ",\"container\":null"] {
        let mut projector = projector();
        let events = projector
            .push(message_start_stream(extra).as_bytes())
            .expect("message_start with an absent or null container");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        );
        projector.finish_eof().expect("EOF");
        projector
            .into_terminal_projection()
            .expect("terminal projection");
    }

    // A reported container is the hosted artifact identity, so it binds
    // exactly like the `message_delta` container.
    let hosted_container =
        ",\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-09-08T00:00:00Z\"}";
    let mut hosted = projector_with_hosted(true);
    hosted
        .push(message_start_stream(hosted_container).as_bytes())
        .expect("hosted message_start container");
    assert_eq!(hosted.container_id.as_deref(), Some("container_1"));

    // Unadmitted or malformed containers still fail closed.
    assert_eq!(
        projector()
            .push(message_start_stream(hosted_container).as_bytes())
            .expect_err("client-only requests may not receive a container")
            .kind(),
        MessagesStreamErrorKind::InvalidEvent
    );
    assert_eq!(
        projector_with_hosted(true)
            .push(
                message_start_stream(
                    ",\"container\":{\"id\":\"\",\"expires_at\":\"2026-09-08T00:00:00Z\"}"
                )
                .as_bytes()
            )
            .expect_err("empty container ID")
            .kind(),
        MessagesStreamErrorKind::InvalidEvent
    );
    assert_eq!(
        projector_with_hosted(true)
            .push(message_start_stream(",\"container\":{\"id\":\"container_1\"}").as_bytes())
            .expect_err("container missing its documented expiry")
            .kind(),
        MessagesStreamErrorKind::InvalidResponse
    );

    // A truly unknown top-level message field is still rejected.
    assert_eq!(
        projector()
            .push(message_start_stream(",\"future_field\":1").as_bytes())
            .expect_err("unknown message_start field")
            .kind(),
        MessagesStreamErrorKind::InvalidResponse
    );
}

#[test]
fn claude_oauth_message_start_admits_bounded_context_management_only_for_the_costume() {
    let stream = message_start_stream(",\"context_management\":{\"applied_edits\":[]}");
    let mut oauth = claude_oauth_projector();
    let events = oauth
        .push(stream.as_bytes())
        .expect("pinned Claude OAuth message_start context management");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
    );
    oauth.finish_eof().expect("complete Claude OAuth EOF");
    oauth
        .into_terminal_projection()
        .expect("Claude OAuth terminal projection");

    assert_eq!(
        projector()
            .push(stream.as_bytes())
            .expect_err("strict Messages must reject the OAuth-only field")
            .kind(),
        MessagesStreamErrorKind::InvalidResponse
    );
    assert_eq!(
        claude_oauth_projector()
            .push(message_start_stream(",\"context_management\":{\"future_edits\":[]}").as_bytes())
            .expect_err("unknown context management shape")
            .kind(),
        MessagesStreamErrorKind::InvalidResponse
    );
    assert_eq!(
        claude_oauth_projector()
            .push(message_start_stream(",\"future_field\":1").as_bytes())
            .expect_err("unknown message_start field under the costume")
            .kind(),
        MessagesStreamErrorKind::InvalidResponse
    );
}

#[test]
fn claude_prefix_binding_notification_does_not_fail_or_rewrite_the_response() {
    // Body-only native 2.1.284 / Sonnet 5.5 OAuth observation: changing
    // an earlier input prefix while retaining signed thinking produces
    // this notification. Native continuation retains the thinking.
    let notification = serde_json::json!([
        {"type":"thinking_dropped", "path":"messages.2.content.0",
         "reason":"prefix_binding_mismatch"},
        {"type":"thinking_dropped", "path":"messages.17.content.3",
         "reason":"prefix_binding_mismatch"}
    ]);
    let mut notified = claude_oauth_projector();
    notified
        .push(
            message_start_stream(&format!(",\"input_transformations\":{notification}")).as_bytes(),
        )
        .expect("witnessed prefix-binding notification must complete");
    notified.finish_eof().expect("notified complete EOF");
    let mut baseline = claude_oauth_projector();
    baseline
        .push(message_start_stream("").as_bytes())
        .expect("baseline response");
    baseline.finish_eof().expect("baseline complete EOF");
    let (notified, model, stop) = notified
        .into_terminal_projection()
        .expect("notified projection")
        .into_parts();
    let (baseline, baseline_model, baseline_stop) = baseline
        .into_terminal_projection()
        .expect("baseline projection")
        .into_parts();
    assert_eq!(model, baseline_model);
    assert_eq!(stop, baseline_stop);
    assert_eq!(notified.output().len(), 1);
    assert_eq!(notified.output()[0].blocks(), baseline.output()[0].blocks());
    assert_eq!(notified.finish(), baseline.finish());
    assert_eq!(notified.usage(), baseline.usage());
    assert!(
        projector()
            .push(
                message_start_stream(&format!(",\"input_transformations\":{notification}"))
                    .as_bytes()
            )
            .is_err()
    );

    for invalid in [
        serde_json::json!({"type":"clear","path":"messages.2.content.0","reason":"prefix_binding_mismatch"}),
        serde_json::json!({"type":"thinking_dropped","path":"messages.2.content.0","reason":"unknown"}),
        serde_json::json!({"type":"thinking_dropped","path":"messages.2","reason":"prefix_binding_mismatch"}),
        serde_json::json!({"type":"thinking_dropped","path":"messages.-2.content.0","reason":"prefix_binding_mismatch"}),
        serde_json::json!({"type":"thinking_dropped","path":"messages.2.content.0.text","reason":"prefix_binding_mismatch"}),
        serde_json::json!({"type":"thinking_dropped","path":"messages.2.content.0","reason":"prefix_binding_mismatch","future":true}),
        serde_json::json!({"type":"thinking_dropped","path":2,"reason":"prefix_binding_mismatch"}),
    ] {
        let stream = message_start_stream(&format!(",\"input_transformations\":[{invalid}]"));
        let error = claude_oauth_projector()
            .push(stream.as_bytes())
            .expect_err("unqualified transformation must fail closed");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
        assert!(
            !error.to_string().contains("messages.2"),
            "no raw path in diagnostics"
        );
    }
}

#[test]
fn claude_opus_message_start_admits_null_diagnostics_and_empty_transformations() {
    let witnessed = message_start_stream(",\"diagnostics\":null,\"input_transformations\":[]");
    let mut oauth = claude_oauth_projector();
    let events = oauth
        .push(witnessed.as_bytes())
        .expect("observed Opus envelope");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
    );
    oauth.finish_eof().expect("complete OAuth EOF");
    assert!(oauth.into_terminal_projection().is_ok());

    for extension in [
        ",\"input_transformations\":[{\"type\":\"clear\"}]",
        ",\"input_transformations\":null",
        ",\"future_field\":1",
    ] {
        let error = claude_oauth_projector()
            .push(message_start_stream(extension).as_bytes())
            .expect_err("unwitnessed envelope must fail closed");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
    }
    // The strict route admits the same diagnostics object as Claude OAuth.
    // Input transformations and any other message field stay refused.
    projector()
        .push(message_start_stream(",\"diagnostics\":null").as_bytes())
        .expect("the API's null diagnostics");
    for extension in [",\"input_transformations\":[]", ",\"future_field\":1"] {
        assert_eq!(
            projector()
                .push(message_start_stream(extension).as_bytes())
                .expect_err("other Messages routes stay strict")
                .kind(),
            MessagesStreamErrorKind::InvalidResponse,
            "{extension}"
        );
    }
}

#[test]
fn message_start_admits_cache_miss_diagnostics_without_changing_the_turn() {
    let known = [
        (
            "model_changed",
            plakous_model::PromptCacheMissReason::ModelChanged,
        ),
        (
            "system_changed",
            plakous_model::PromptCacheMissReason::SystemChanged,
        ),
        (
            "tools_changed",
            plakous_model::PromptCacheMissReason::ToolsChanged,
        ),
        (
            "messages_changed",
            plakous_model::PromptCacheMissReason::MessagesChanged,
        ),
        (
            "previous_message_not_found",
            plakous_model::PromptCacheMissReason::PreviousMessageNotFound,
        ),
        (
            "unavailable",
            plakous_model::PromptCacheMissReason::Unavailable,
        ),
        (
            "private-sentinel",
            plakous_model::PromptCacheMissReason::Other,
        ),
        ("", plakous_model::PromptCacheMissReason::Other),
    ];
    for (kind, reason) in known {
        let extra = format!(
            r#","diagnostics":{{"ignored":true,"cache_miss_reason":{{"type":"{kind}","cache_missed_input_tokens":587,"private-sentinel":true}}}}"#
        );
        for mut stream in [projector(), claude_oauth_projector()] {
            let events = stream
                .push(message_start_stream(&extra).as_bytes())
                .expect("known cache-miss diagnostics");
            let diagnosis = events.iter().find_map(|event| match event {
                ModelEvent::PromptCacheDiagnosis(diagnosis) => Some(*diagnosis),
                _ => None,
            });
            let diagnosis = diagnosis.expect("diagnosis event");
            assert_eq!(diagnosis.reason, reason);
            assert_eq!(diagnosis.missed_input_tokens, Some(587));
            let rendered = format!("{diagnosis:?}");
            assert!(!rendered.contains("private-sentinel"));
            assert!(!rendered.contains("msg_"));
            stream.finish_eof().expect("EOF");
            let (response, _, _) = stream
                .into_terminal_projection()
                .expect("terminal")
                .into_parts();
            assert_eq!(response.usage(), baseline_usage());
        }
    }
    for extra in [
        r#","diagnostics":null"#,
        r#","diagnostics":{"cache_miss_reason":null}"#,
        r#","diagnostics":{}"#,
        r#","diagnostics":{"advisory":1}"#,
    ] {
        for mut stream in [projector(), claude_oauth_projector()] {
            let events = stream
                .push(message_start_stream(extra).as_bytes())
                .expect("diagnostics with nothing to record");
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, ModelEvent::PromptCacheDiagnosis(_)))
            );
            stream.finish_eof().expect("EOF");
            let (response, _, _) = stream
                .into_terminal_projection()
                .expect("terminal")
                .into_parts();
            assert_eq!(response.usage(), baseline_usage());
        }
    }
    let zero = r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":0}}"#;
    let events = projector()
        .push(message_start_stream(zero).as_bytes())
        .expect("zero missed tokens");
    let diagnosis = events.iter().find_map(|event| match event {
        ModelEvent::PromptCacheDiagnosis(diagnosis) => Some(*diagnosis),
        _ => None,
    });
    assert_eq!(
        diagnosis.expect("zero diagnosis").missed_input_tokens,
        Some(0)
    );
}

#[test]
fn missing_cache_miss_count_is_admitted_and_u64_max_is_kept() {
    let omitted = r#","diagnostics":{"cache_miss_reason":{"type":"system_changed","ignored":"private-sentinel"}}"#;
    for mut stream in [projector(), claude_oauth_projector()] {
        let events = stream
            .push(message_start_stream(omitted).as_bytes())
            .expect("missing count is admitted");
        let diagnosis = events.iter().find_map(|event| match event {
            ModelEvent::PromptCacheDiagnosis(diagnosis) => Some(*diagnosis),
            _ => None,
        });
        let diagnosis = diagnosis.expect("omitted count");
        assert_eq!(
            diagnosis.reason,
            plakous_model::PromptCacheMissReason::SystemChanged
        );
        assert_eq!(diagnosis.missed_input_tokens, None);
        let rendered = format!("{diagnosis:?}");
        assert!(!rendered.contains("private-sentinel"));
        assert!(!rendered.contains("msg_"));
        stream.finish_eof().expect("EOF");
        let (response, _, _) = stream
            .into_terminal_projection()
            .expect("terminal")
            .into_parts();
        assert_eq!(response.usage(), baseline_usage());
    }
    let max_tokens = format!(
        r#","diagnostics":{{"cache_miss_reason":{{"type":"unavailable","cache_missed_input_tokens":{}}}}}"#,
        u64::MAX
    );
    let events = projector()
        .push(message_start_stream(&max_tokens).as_bytes())
        .expect("u64 max is a non-negative integer");
    let diagnosis = events.iter().find_map(|event| match event {
        ModelEvent::PromptCacheDiagnosis(diagnosis) => Some(*diagnosis),
        _ => None,
    });
    assert_eq!(
        diagnosis.expect("max diagnosis").missed_input_tokens,
        Some(u64::MAX)
    );
}

#[test]
fn malformed_cache_miss_diagnostics_record_nothing_and_the_turn_proceeds() {
    let cases = [
        r#","diagnostics":"private-sentinel""#,
        r#","diagnostics":["private-sentinel"]"#,
        r#","diagnostics":{"cache_miss_reason":"private-sentinel"}"#,
        r#","diagnostics":{"cache_miss_reason":{"cache_missed_input_tokens":1}}"#,
        r#","diagnostics":{"cache_miss_reason":{"type":null,"cache_missed_input_tokens":1}}"#,
        r#","diagnostics":{"cache_miss_reason":{"type":1,"cache_missed_input_tokens":1}}"#,
        r#","diagnostics":{"cache_miss_reason":{"type":true,"cache_missed_input_tokens":1}}"#,
        r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":-1}}"#,
        r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":1.5}}"#,
        r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":"587"}}"#,
        r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":18446744073709551616}}"#,
        r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":null,"private-sentinel":true}}"#,
    ];
    for extra in cases {
        for mut stream in [projector(), claude_oauth_projector()] {
            let events = stream
                .push(message_start_stream(extra).as_bytes())
                .unwrap_or_else(|error| panic!("turn must proceed for {extra}: {error}"));
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, ModelEvent::PromptCacheDiagnosis(_))),
                "{extra}"
            );
            assert!(
                !format!("{events:?}").contains("private-sentinel"),
                "{extra}"
            );
            stream.finish_eof().expect("EOF");
            let (response, _, _) = stream
                .into_terminal_projection()
                .expect("terminal")
                .into_parts();
            assert_eq!(response.usage(), baseline_usage());
        }
    }
}

#[test]
fn unknown_message_start_fields_still_fail_closed() {
    let cases = [
        r#","future_field":1"#,
        r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":1,"private-sentinel":true}},"future_field":1"#,
        r#","diagnostics":"private-sentinel","future_field":1"#,
    ];
    for extra in cases {
        for mut stream in [projector(), claude_oauth_projector()] {
            let error = stream
                .push(message_start_stream(extra).as_bytes())
                .expect_err("unknown message_start field must fail closed");
            assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
            assert!(
                error.message().contains("invalid"),
                "missing invalid in {}",
                error.message()
            );
            assert!(!error.message().contains("private-sentinel"));
            assert!(!error.message().contains("msg_"));
        }
    }
}

#[test]
fn openrouter_rejects_message_diagnostics() {
    let extra = r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":1,"private-sentinel":true}}"#;
    let error = openrouter_projector()
        .push(message_start_stream(extra).as_bytes())
        .expect_err("OpenRouter rejects diagnostics");
    assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
    assert!(!error.message().contains("private-sentinel"));
    assert!(!error.message().contains("msg_"));
}

fn baseline_usage() -> Option<plakous_model::Usage> {
    let mut stream = projector();
    stream
        .push(message_start_stream("").as_bytes())
        .expect("baseline");
    stream.finish_eof().expect("baseline EOF");
    let (response, _, _) = stream
        .into_terminal_projection()
        .expect("baseline terminal")
        .into_parts();
    response.usage()
}

#[test]
fn claude_opus_observed_thinking_stream_projects_to_terminal() {
    // Synthetic values preserve the 2.1.280 Opus message and event shapes.
    // No native prompt, message identity, signature, or session data enters
    // the fixture.
    let stream = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_synthetic\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5-5\",\"content\":[],\"container\":null,\"context_management\":null,\"diagnostics\":null,\"input_transformations\":[],\"stop_details\":null,\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\",\"estimated_tokens\":null}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_synthetic\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"stop_details\":null,\"container\":null},\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"iterations\":[]},\"context_management\":{\"applied_edits\":[]}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let mut oauth = claude_oauth_projector();
    let events = oauth.push(stream.as_bytes()).expect("observed OAuth shape");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
    );
    oauth.finish_eof().expect("complete OAuth EOF");
    assert!(oauth.into_terminal_projection().is_ok());
}

#[test]
fn parser_preserves_absent_and_explicit_direct_native_forms() {
    let dialect = MessagesStreamDialectHooks::new(
        "code_execution_20260120",
        NamespaceId::new("anthropic.programmatic_tool").expect("namespace"),
    );
    assert_eq!(
        parse_tool_caller(None, &dialect).expect("absent direct"),
        ToolCaller::AbsentDirect
    );
    let explicit = serde_json::value::RawValue::from_string(r#"{"type":"direct"}"#.to_owned())
        .expect("caller");
    assert_eq!(
        parse_tool_caller(Some(explicit.as_ref()), &dialect).expect("explicit direct"),
        ToolCaller::ExplicitDirect
    );
}

#[test]
fn client_tools_only_hook_has_no_hosted_program_identity() {
    let dialect = MessagesStreamDialectHooks::client_tools_only();
    assert_eq!(
        parse_tool_caller(None, &dialect).expect("absent direct"),
        ToolCaller::AbsentDirect
    );
    let explicit = serde_json::value::RawValue::from_string(r#"{"type":"direct"}"#.to_owned())
        .expect("caller");
    assert_eq!(
        parse_tool_caller(Some(explicit.as_ref()), &dialect).expect("explicit direct"),
        ToolCaller::ExplicitDirect
    );
    let hosted = serde_json::value::RawValue::from_string(
        r#"{"type":"code_execution_20260120","tool_id":"srv_1"}"#.to_owned(),
    )
    .expect("caller");
    let error = parse_tool_caller(Some(hosted.as_ref()), &dialect)
        .expect_err("client-only hook must reject hosted callers");
    assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
}

#[test]
fn openrouter_v1_projects_real_shaped_lifecycle_and_derives_request_id() {
    let mut projector = openrouter_projector();
    let events = projector
        .push(OPENROUTER_STREAM.as_bytes())
        .expect("reviewed OpenRouter stream");
    projector.finish_eof().expect("complete OpenRouter EOF");
    assert_eq!(
        projector
            .provider_request_id()
            .map(plakous_model::ModelProviderRequestId::as_str),
        Some("gen-sanitized-request-01")
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::ResponseStarted(started)
            if started.provider_request_id() == Some("gen-sanitized-request-01")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::ResponseCompleted(response)
            if response.provider_request_id() == Some("gen-sanitized-request-01")
                && response.finish().reason() == FinishReason::Length
    )));
    projector
        .into_terminal_projection()
        .expect("terminal projection remains message_stop-authoritative");
}

#[test]
fn messages_dialects_project_thinking_usage_canonically() {
    let openrouter_stream =
        OPENROUTER_STREAM.replacen("\"thinking_tokens\":0", "\"thinking_tokens\":1", 1);
    let mut openrouter = openrouter_projector();
    let openrouter_events = openrouter
        .push(openrouter_stream.as_bytes())
        .expect("OpenRouter thinking usage");
    assert!(openrouter_events.iter().any(|event| matches!(
        event,
        ModelEvent::Usage(update) if update.usage().reasoning_tokens() == 1
    )));

    let claude_stream = CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM.replacen(
        "\"output_tokens\":1}",
        "\"output_tokens\":1,\"output_tokens_details\":{\"thinking_tokens\":1}}",
        1,
    );
    let mut claude = claude_oauth_projector();
    let claude_events = claude
        .push(claude_stream.as_bytes())
        .expect("Claude OAuth thinking usage");
    assert!(claude_events.iter().any(|event| matches!(
        event,
        ModelEvent::Usage(update) if update.usage().reasoning_tokens() == 1
    )));
}

#[test]
fn openrouter_v1_projects_interleaved_blocks_in_provider_index_order() {
    let mut openrouter = openrouter_projector();
    openrouter
        .push(OPENROUTER_INTERLEAVED_TOOL_STREAM.as_bytes())
        .expect("reviewed interleaved OpenRouter stream");
    openrouter.finish_eof().expect("complete OpenRouter EOF");
    let (response, _, _) = openrouter
        .into_terminal_projection()
        .expect("terminal projection")
        .into_parts();
    assert!(matches!(
        response.output()[0].blocks()[0],
        ContentBlock::Text(_)
    ));
    assert!(matches!(
        response.output()[0].blocks()[1],
        ContentBlock::ToolCall(_)
    ));
    assert!(matches!(
        response.output()[0].blocks()[2],
        ContentBlock::Text(_)
    ));

    let error = projector()
        .push(OPENROUTER_INTERLEAVED_TOOL_STREAM.as_bytes())
        .expect_err("strict Messages must retain its single-open-block grammar");
    assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
}

#[test]
fn openrouter_empty_unsigned_thinking_preserves_following_block_indexes() {
    let stream = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_empty_thinking\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"ok\",\"citations\":[]}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let mut projector = openrouter_projector();
    projector.push(stream.as_bytes()).expect("valid stream");
    projector.finish_eof().expect("terminal EOF");
    let (response, _, _) = projector
        .into_terminal_projection()
        .expect("terminal projection")
        .into_parts();
    let blocks = response.output()[0].blocks();
    assert!(matches!(
        &blocks[0],
        ContentBlock::Reasoning(block)
            if block.visibility() == plakous_model::ReasoningVisibility::Omitted
    ));
    assert!(matches!(&blocks[1], ContentBlock::Text(block) if block.text() == "ok"));
}

#[test]
fn strict_messages_rejects_openrouter_additions() {
    let error = projector()
        .push(OPENROUTER_STREAM.as_bytes())
        .expect_err("strict grammar must remain unchanged");
    assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
}

#[test]
fn openrouter_v1_accepts_optional_exact_done_and_rejects_unreviewed_additions() {
    let without_done = OPENROUTER_STREAM
        .split("event: data\ndata: [DONE]")
        .next()
        .expect("prefix");
    let mut missing = openrouter_projector();
    missing
        .push(without_done.as_bytes())
        .expect("message_stop remains terminal authority");
    missing
        .finish_eof()
        .expect("message_stop is sufficient at EOF");
    missing
        .into_terminal_projection()
        .expect("message_stop remains terminal authority");

    let mut non_null_context = openrouter_projector();
    let stream = OPENROUTER_STREAM.replacen(
        "\"context_management\":null",
        "\"context_management\":{}",
        1,
    );
    assert_eq!(
        non_null_context
            .push(stream.as_bytes())
            .expect_err("unreviewed context semantic")
            .kind(),
        MessagesStreamErrorKind::UnsupportedSemantic
    );

    let mut unknown_usage = openrouter_projector();
    let stream = OPENROUTER_STREAM.replacen("\"cost\":0", "\"future_cost\":0", 1);
    unknown_usage
        .push(stream.as_bytes())
        .expect("an additive usage key is ignored");

    let mut unknown_stop = openrouter_projector();
    let stream = OPENROUTER_STREAM.replacen("\"max_tokens\"", "\"future_stop\"", 1);
    assert_eq!(
        unknown_stop
            .push(stream.as_bytes())
            .expect_err("unknown terminal enum")
            .kind(),
        MessagesStreamErrorKind::UnsupportedSemantic
    );

    let mut decreasing_usage = openrouter_projector();
    let stream = OPENROUTER_STREAM.replacen("\"input_tokens\":0", "\"input_tokens\":20", 1);
    assert_eq!(
        decreasing_usage
            .push(stream.as_bytes())
            .expect_err("decreasing cumulative usage")
            .kind(),
        MessagesStreamErrorKind::InvalidAccounting
    );
}

fn push(sse: &str) -> Vec<ModelEvent> {
    let mut projector = projector();
    projector.push(sse.as_bytes()).expect("stream projects")
}

fn pause_stream(container: bool, client_tool: bool) -> String {
    let container = if container {
        "\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-08-23T12:00:00Z\"},"
    } else {
        ""
    };
    let client = if client_tool {
        concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"weather\",\"input\":{}}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        )
    } else {
        ""
    };
    format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_pause\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n",
            "event: content_block_start\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"code_execution\",\"input\":{{}}}}}}\n\n",
            "event: content_block_stop\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "{}",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{{}\"stop_reason\":\"pause_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        client, container
    )
}

#[test]
fn hosted_pause_maps_to_provider_continuation() {
    let mut projector = projector_with_hosted(true);
    let events = projector
        .push(pause_stream(true, false).as_bytes())
        .expect("admitted hosted pause");
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::ResponseCompleted(response)
            if response.finish().reason() == FinishReason::ProviderContinuation
    )));
}

#[test]
fn hosted_pause_rejects_missing_container_and_client_tool_contradictions() {
    let mut missing_container = projector_with_hosted(true);
    let error = missing_container
        .push(pause_stream(false, false).as_bytes())
        .expect_err("pause without container");
    assert!(error.to_string().contains("pause_turn"));

    let mut client_tool = projector_with_hosted(true);
    let error = client_tool
        .push(pause_stream(true, true).as_bytes())
        .expect_err("pause with client tool");
    assert!(
        error
            .to_string()
            .contains("stop_reason=pause_turn client_proposals=1 provider_proposals=1")
    );
}

#[test]
fn hosted_container_binds_on_message_delta_only_and_rejects_drift() {
    let mut wrong_start = projector_with_hosted(true);
    let start_container = pause_stream(true, false).replacen(
        "\"content\":[]",
        "\"content\":[],\"container\":{\"id\":\"wrong\",\"expires_at\":\"2026-08-23T12:00:00Z\"}",
        1,
    );
    assert!(wrong_start.push(start_container.as_bytes()).is_err());

    let mut wrong_active = projector_with_hosted(true);
    wrong_active.prepared.expected_container = Some("container_prior".to_owned());
    let error = wrong_active
        .push(pause_stream(true, false).as_bytes())
        .expect_err("active container drift");
    assert!(error.to_string().contains("active artifact lineage"));

    let mut conflicting = projector_with_hosted(true);
    let stream = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_conflict\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-08-23T12:00:00Z\"}},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"container\":{\"id\":\"container_2\",\"expires_at\":\"2026-08-23T12:00:00Z\"},\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    );
    let error = conflicting
        .push(stream.as_bytes())
        .expect_err("conflicting repeated container");
    assert!(error.to_string().contains("conflicting container"));
}

#[test]
fn empty_placeholder_signature_is_unbound_until_signature_delta() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_real\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = push(sse);
    assert!(
        events.iter().any(|event| matches!(
            event,
            ModelEvent::ReasoningReplayBound(bound)
                if bound.replay().payload().get().contains("sig_real")
        )),
        "empty start signature must not hide the later signature_delta: {events:?}"
    );
}

#[test]
fn empty_thinking_delta_and_repeat_signature_are_noops() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"sig_one\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_\\u006fne\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = push(sse);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::BlockStarted(_))),
        "empty thinking must still open a reasoning block"
    );
}

#[test]
fn conflicting_repeat_signature_fails_closed() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"sig_one\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_two\"}}\n\n",
    );
    let error = projector()
        .push(sse.as_bytes())
        .expect_err("conflicting signature must fail");
    assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
}

#[test]
fn unadvertised_tool_use_projects_as_local_requested() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_hidden\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_hidden\",\"name\":\"Write\",\"input\":{}}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = push(sse);
    assert!(
        events.iter().any(|event| matches!(
            event,
            ModelEvent::BlockStarted(started)
                if matches!(
                    started.block(),
                    plakous_model::BlockStart::ToolCall { tool, owner, state, .. }
                        if tool == &ToolName::new("Write").expect("name")
                            && *owner == plakous_model::ToolExecutionOwner::Local
                            && *state == plakous_model::ToolCallState::Requested
                )
        )),
        "hidden Write must materialize as a local requested tool call"
    );
}

use super::{MessageDeltaUsage, ThinkingDeltaWire, UsageWire, parse_thinking_delta};

const LIVE_SUBSCRIPTION_USAGE: &str = r#"{
    "input_tokens": 2,
    "cache_creation_input_tokens": 51132,
    "cache_read_input_tokens": 0,
    "output_tokens": 4,
    "output_tokens_details": { "thinking_tokens": 0 },
    "server_tool_use": { "web_search_requests": 0, "web_fetch_requests": 0 },
    "service_tier": "standard",
    "cache_creation": {
        "ephemeral_1h_input_tokens": 51132,
        "ephemeral_5m_input_tokens": 0
    },
    "inference_geo": "not_available",
    "iterations": [{
        "input_tokens": 2,
        "output_tokens": 4,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 51132,
        "cache_creation": {
            "ephemeral_5m_input_tokens": 0,
            "ephemeral_1h_input_tokens": 51132
        },
        "type": "message"
    }],
    "speed": "standard"
}"#;

#[test]
fn live_claude_oauth_usage_admits_subscription_receipt() {
    let usage: UsageWire = serde_json::from_str(LIVE_SUBSCRIPTION_USAGE).expect("pinned usage");
    assert_eq!(usage.input_tokens, 2);
    assert_eq!(usage.cache_creation_input_tokens, 51132);
    assert_eq!(usage.output_tokens, 4);
    let provider = usage.into_provider();
    assert_eq!(provider.cache_creation_input_tokens, 51132);
    assert_eq!(provider.cache_read_input_tokens, 0);
}

#[test]
fn messages_usage_preserves_cumulative_thinking_breakdown() {
    let initial: UsageWire = serde_json::from_str(
        r#"{"input_tokens":7,"output_tokens":10,"output_tokens_details":{"thinking_tokens":4}}"#,
    )
    .expect("initial usage");
    let provider = initial.into_provider();
    let canonical = provider.into_canonical().expect("canonical initial usage");
    assert_eq!(canonical.output_tokens(), 10);
    assert_eq!(canonical.reasoning_tokens(), 4);

    let update: MessageDeltaUsage = serde_json::from_str(
        r#"{"output_tokens":12,"output_tokens_details":{"thinking_tokens":5}}"#,
    )
    .expect("terminal usage");
    let provider = provider.merge(update.into_family()).expect("usage update");
    let canonical = provider.into_canonical().expect("canonical terminal usage");
    assert_eq!(canonical.output_tokens(), 12);
    assert_eq!(canonical.reasoning_tokens(), 5);
}

#[test]
fn messages_usage_rejects_inconsistent_thinking_accounting() {
    let usage: UsageWire = serde_json::from_str(
        r#"{"input_tokens":1,"output_tokens":2,"output_tokens_details":{"thinking_tokens":3}}"#,
    )
    .expect("typed usage");
    assert_eq!(
        usage.into_provider().into_canonical(),
        Err("Messages thinking usage exceeded output usage")
    );

    let initial: UsageWire = serde_json::from_str(
        r#"{"input_tokens":1,"output_tokens":4,"output_tokens_details":{"thinking_tokens":3}}"#,
    )
    .expect("initial usage");
    let update: MessageDeltaUsage = serde_json::from_str(
        r#"{"output_tokens":5,"output_tokens_details":{"thinking_tokens":2}}"#,
    )
    .expect("decreasing thinking update");
    assert_eq!(
        initial.into_provider().merge(update.into_family()),
        Err("Messages cumulative usage decreased")
    );
}

#[test]
fn unknown_usage_keys_are_ignored_and_accounting_is_unchanged() {
    let usage = serde_json::from_str::<UsageWire>(
        r#"{"input_tokens":1,"output_tokens":2,"future_counter":{"x":1},"speed":"fast"}"#,
    )
    .expect("additive usage keys");
    let provider = usage.into_provider();
    assert_eq!((provider.input_tokens, provider.output_tokens), (1, 2));
    serde_json::from_str::<UsageWire>(r#"{"input_tokens":"1","output_tokens":1}"#)
        .expect_err("a known counter keeps its type");
}

#[test]
fn live_claude_oauth_message_delta_usage_admits_subscription_receipt() {
    let usage: MessageDeltaUsage = serde_json::from_str(
        r#"{
            "output_tokens": 8,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 0,
                "ephemeral_1h_input_tokens": 0
            },
            "service_tier": "standard",
            "inference_geo": "not_available",
            "speed": "standard"
        }"#,
    )
    .expect("pinned message_delta usage");
    assert_eq!(usage.output_tokens, 8);
}

#[test]
fn claude_code_empty_thinking_delta_is_observed() {
    let delta: ThinkingDeltaWire =
        serde_json::from_str(r#"{"type":"thinking_delta","thinking":""}"#)
            .expect("empty thinking delta is observed on Claude Code");
    assert_eq!(delta.thinking, "");
}

#[test]
fn claude_opus_thinking_estimate_is_a_nonsemantic_unsigned_hint() {
    let witnessed = r#"{"type":"thinking_delta","thinking":"","estimated_tokens":null}"#;
    assert!(parse_thinking_delta(witnessed, MessagesStreamExtensions::ClaudeOauthV1).is_ok());
    for extension in [
        MessagesStreamExtensions::Strict,
        MessagesStreamExtensions::OpenRouterV1,
    ] {
        assert!(parse_thinking_delta(witnessed, extension).is_err());
    }
    let estimate = r#"{"type":"thinking_delta","thinking":"step","estimated_tokens":42}"#;
    assert_eq!(
        parse_thinking_delta(estimate, MessagesStreamExtensions::ClaudeOauthV1)
            .expect("unsigned estimate is an ignored hint")
            .thinking,
        "step"
    );
    assert!(parse_thinking_delta(estimate, MessagesStreamExtensions::Strict).is_err());
    for (invalid, shape) in [
        ("-1", "non-unsigned number"),
        ("1.5", "non-unsigned number"),
        ("\"private-sentinel\"", "string"),
        ("true", "boolean"),
        ("{}", "object"),
    ] {
        let delta = format!(
            "{{\"type\":\"thinking_delta\",\"thinking\":\"\",\"estimated_tokens\":{invalid}}}"
        );
        let error = parse_thinking_delta(&delta, MessagesStreamExtensions::ClaudeOauthV1)
            .err()
            .expect("unsupported shape must be rejected");
        assert!(error.message().contains(shape));
        assert!(!error.message().contains("private-sentinel"));
    }
}
