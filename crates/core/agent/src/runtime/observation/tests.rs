#![allow(
    clippy::expect_used,
    reason = "unit tests use expect for fixture construction"
)]
use plakous_model::{
    CallId, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner, ToolName,
};
use std::fmt::Write as _;

#[test]
fn observation_redacts_before_subject_and_argument_bounds() {
    let command = format!("{}token=abcdefgh", "x ".repeat(112));
    let call = ToolCallBlock::new(
        "00000000-0000-7000-8000-000000000904"
            .parse::<CallId>()
            .expect("call ID"),
        ToolName::new("exec_command").expect("name"),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-command".into()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )
        .expect("correlation"),
        serde_json::json!({"cmd": command, "input_tokens": 123, "access_token": "short"})
            .to_string(),
    )
    .expect("call");
    let observation = super::tool_call_observation(&call);
    let subject = observation.subject().expect("subject");
    assert!(subject.contains("token=[red"), "{subject}");
    assert!(!subject.contains("abcdefgh"));
    let preview: serde_json::Value =
        serde_json::from_str(observation.arguments_preview()).expect("JSON preview");
    assert_eq!(preview["input_tokens"], 123);
    assert_eq!(preview["access_token"], "[redacted]");
    assert!(call.raw_arguments().contains("abcdefgh"));
    assert!(call.raw_arguments().contains("short"));
}

#[test]
fn long_patch_subject_keeps_complete_diffstat() {
    let patch = format!(
        "*** Begin Patch\n*** Update File: file.rs\n@@\n-old\n{}*** End Patch",
        "+new\n".repeat(600)
    );
    let call = ToolCallBlock::new(
        "00000000-0000-7000-8000-000000000903"
            .parse::<CallId>()
            .expect("call ID"),
        ToolName::new("apply_patch").expect("name"),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-patch".into()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )
        .expect("correlation"),
        serde_json::json!({"patch":patch}).to_string(),
    )
    .expect("call");
    assert_eq!(
        super::tool_call_observation(&call).subject(),
        Some("file.rs +600/-1")
    );
}

#[test]
fn subjects_are_single_line_and_bounded_without_marker_text() {
    let input = format!("first\nsecond\r\n{}", "界".repeat(200));
    let call = ToolCallBlock::new(
        "00000000-0000-7000-8000-000000000902"
            .parse::<CallId>()
            .expect("call ID"),
        ToolName::new("read_file").expect("name"),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-read".into()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )
        .expect("correlation"),
        serde_json::json!({"path": input}).to_string(),
    )
    .expect("call");
    let observation = super::tool_call_observation(&call);
    let subject = observation.subject().expect("subject");
    assert!(subject.len() <= 240);
    assert!(subject.contains('↵'));
    assert!(subject.ends_with('…'));
    assert!(!subject.contains('\n'));
    assert!(!subject.contains("preview truncated"));
}

#[test]
fn local_composition_exec_admission_unquotes_the_canonical_custom_tool_string() {
    use plakous_model::ToolInputContract;

    let source = "const r = await tools.read_file({path:'TASK.md'}); text(r)";
    let call = ToolCallBlock::new(
        "00000000-0000-7000-8000-000000000901"
            .parse::<CallId>()
            .expect("call ID"),
        ToolName::new("exec").expect("tool name"),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-exec".to_owned()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )
        .expect("correlation"),
        serde_json::to_string(source).expect("custom input"),
    )
    .expect("tool call");
    assert_eq!(
        call.raw_arguments(),
        serde_json::to_string(source).expect("custom input encoding")
    );
    assert_eq!(
        crate::runtime::composition::projection::local_composition_exec_registry_arguments(
            &call,
            &ToolInputContract::utf8(4_096).expect("UTF-8 contract"),
        )
        .expect("source"),
        source
    );
}

#[test]
fn local_composition_exec_admission_preserves_a_structured_function_object() {
    use plakous_model::ToolInputContract;

    let raw = r#"{"source":"yield once","max_output_tokens":64}"#;
    let call = ToolCallBlock::new(
        "00000000-0000-7000-8000-000000000902"
            .parse::<CallId>()
            .expect("call ID"),
        ToolName::new("execute").expect("tool name"),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-execute".to_owned()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )
        .expect("correlation"),
        raw,
    )
    .expect("tool call");
    let contract = ToolInputContract::structured(serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "source": {"type": "string"},
            "max_output_tokens": {"type": "integer", "minimum": 0}
        },
        "required": ["source"],
        "additionalProperties": false
    }))
    .expect("structured contract");
    assert_eq!(
        crate::runtime::composition::projection::local_composition_exec_registry_arguments(
            &call, &contract
        )
        .expect("structured input"),
        raw
    );
    let error = crate::runtime::composition::projection::local_composition_exec_registry_arguments(
        &call,
        &ToolInputContract::utf8(4_096).expect("UTF-8 contract"),
    )
    .expect_err("Codex UTF-8 must still reject a function object");
    assert_eq!(
        error.message().as_str(),
        "local composition exec input is not a custom UTF-8 string"
    );
}

#[test]
fn local_composition_child_call_identity_is_deterministic_and_digest_sensitive() {
    use plakous_model::{Digest, DigestDomain};

    let left = Digest::sha256(DigestDomain::ProviderOperation, 3, &[b"left"]).expect("left digest");
    let right =
        Digest::sha256(DigestDomain::ProviderOperation, 3, &[b"right"]).expect("right digest");
    assert_eq!(
        crate::runtime::composition::projection::local_composition_child_call_id(left)
            .expect("first identity"),
        crate::runtime::composition::projection::local_composition_child_call_id(left)
            .expect("duplicate identity")
    );
    assert_ne!(
        crate::runtime::composition::projection::local_composition_child_call_id(left)
            .expect("left identity"),
        crate::runtime::composition::projection::local_composition_child_call_id(right)
            .expect("right identity")
    );
}

#[test]
fn local_composition_nested_owner_resolves_only_the_exact_projected_process_names() {
    use std::sync::Arc;

    use plakous_compose::{
        CodexMinimalLocalCodeModeV1, CodexNativeLocalCodeModeV1, standard_coding_packs_in_memory,
    };
    use plakous_model::{Digest, DigestDomain, IntegrationClass, RunId};
    use plakous_tools::{
        AlwaysAllowPolicy, ExecutionEnvironment, ExecutionEnvironmentConfig,
        ExecutionEnvironmentScope, ProviderModelProjectionIdentityV2, ToolRegistryBuilder,
        register_packs,
    };

    let mut builder = ToolRegistryBuilder::default();
    register_packs(
        &standard_coding_packs_in_memory().expect("coding packs"),
        &mut builder,
    )
    .expect("process registrations");
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder).expect("composition markers");
    let registry = builder.freeze();
    let effective_digest = Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"nested-owner-projection-test"],
    )
    .expect("effective identity");
    let effective = serde_json::from_value(
        serde_json::to_value(effective_digest).expect("effective identity value"),
    )
    .expect("typed effective identity");
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "openai.codex.responses",
        "openai.codex_subscription",
        "openai.codex.compat.0.153.3",
        "responses-family",
        "gpt-codex-code-mode-test",
        "gpt-codex-code-mode-test",
        effective,
        1,
    )
    .expect("projection target");
    let descriptor = CodexMinimalLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
    )
    .expect("minimal descriptor");
    let run_id = "00000000-0000-7000-8000-000000000920"
        .parse::<RunId>()
        .expect("run ID");
    let workspace = tempfile::tempdir().expect("workspace");
    let environment = Arc::new(
        ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run_id),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .expect("environment"),
    );
    let runtime =
        crate::tool_runtime::ToolRuntime::new(registry, environment, Arc::new(AlwaysAllowPolicy));

    for exposed in ["exec_command", "write_stdin"] {
        assert_eq!(
            crate::runtime::composition::projection::local_composition_nested_execution_owner(
                &runtime,
                descriptor.nested_projection(),
                &ToolName::new(exposed).expect("projected name"),
            )
            .expect("projected owner"),
            ToolExecutionOwner::Local
        );
    }
    let error = crate::runtime::composition::projection::local_composition_nested_execution_owner(
        &runtime,
        descriptor.nested_projection(),
        &ToolName::new("process_session_start").expect("canonical name"),
    )
    .expect_err("canonical registration must not widen nested authority");
    assert_eq!(error.kind(), crate::AgentErrorKind::InvalidInput);
}

#[test]
fn local_composition_attachment_identity_rejects_tamper() {
    let exact = plakous_model::Digest::sha256(
        plakous_model::DigestDomain::ExecutionEnvironment,
        1,
        &[b"local-composition-attachment-test"],
    )
    .expect("identity");
    let mut attachment = crate::local_composition::LocalCompositionAttachmentIdentity {
        profile: exact,
        artifact: exact,
        binding: exact,
        engine_contract: exact,
        evidence_receipt: None,
    };
    attachment.binding = crate::runtime::build::local_composition_binding_identity(&attachment)
        .expect("binding identity");
    crate::runtime::build::validate_local_composition_attachment_identity(&attachment)
        .expect("exact attachment");

    attachment.artifact = plakous_model::Digest::sha256(
        plakous_model::DigestDomain::ExecutionEnvironment,
        1,
        &[b"tampered-artifact"],
    )
    .expect("tampered identity");
    assert!(
        crate::runtime::build::validate_local_composition_attachment_identity(&attachment).is_err()
    );
}

#[test]
fn local_composition_nested_duplicate_replays_exact_value_and_conflict_stops() {
    use plakous_model::{Digest, DigestDomain};

    let request =
        Digest::sha256(DigestDomain::ProviderOperation, 3, &[b"request"]).expect("request digest");
    let conflict = Digest::sha256(DigestDomain::ProviderOperation, 3, &[b"conflict"])
        .expect("conflict digest");
    let replay = crate::runtime::composition::LocalCompositionNestedReplay {
        request_digest: request,
        request_sequence: 4,
        disposition: crate::runtime::composition::LocalCompositionNestedDisposition::Resolved(
            serde_json::json!({
                "ok": true
            }),
        ),
        waiters: Vec::new(),
    };
    assert_eq!(
        crate::runtime::composition::nested::local_composition_nested_replay(
            Some(&replay),
            request
        )
        .expect("exact duplicate")
        .expect("recorded value"),
        serde_json::json!({"ok": true})
    );
    let error = crate::runtime::composition::nested::local_composition_nested_replay(
        Some(&replay),
        conflict,
    )
    .expect_err("content conflict must stop");
    assert_eq!(error.kind(), crate::AgentErrorKind::OutcomeUnknown);

    let deliveries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut pending = crate::runtime::composition::LocalCompositionNestedReplay {
        request_digest: request,
        request_sequence: 5,
        disposition: crate::runtime::composition::LocalCompositionNestedDisposition::Pending,
        waiters: Vec::new(),
    };
    assert!(
        crate::runtime::composition::nested::attach_local_composition_nested_waiter(
            &mut pending,
            request,
            Box::new(RecordingNestedResponder {
                identity: "duplicate",
                deliveries: std::sync::Arc::clone(&deliveries),
            }),
        )
        .expect("exact pending duplicate")
        .is_none()
    );
    assert_eq!(pending.waiters.len(), 1);
    let error = crate::runtime::composition::nested::attach_local_composition_nested_waiter(
        &mut pending,
        conflict,
        Box::new(RecordingNestedResponder {
            identity: "conflict",
            deliveries,
        }),
    )
    .err()
    .expect("changed content must not become a waiter");
    assert_eq!(error.kind(), crate::AgentErrorKind::OutcomeUnknown);
    assert_eq!(pending.waiters.len(), 1);

    pending.disposition = crate::runtime::composition::LocalCompositionNestedDisposition::Rejected(
        "rejected once".to_owned(),
    );
    let rejected = crate::runtime::composition::nested::local_composition_nested_replay(
        Some(&pending),
        request,
    )
    .expect_err("rejected duplicate returns the recorded rejection");
    assert_eq!(rejected.kind(), crate::AgentErrorKind::InvalidInput);
    assert_eq!(rejected.message(), "rejected once");
    pending.disposition =
        crate::runtime::composition::LocalCompositionNestedDisposition::RecoveryRequired;
    let recovery = crate::runtime::composition::nested::local_composition_nested_replay(
        Some(&pending),
        request,
    )
    .expect_err("recovery duplicate remains recovery-required");
    assert_eq!(recovery.kind(), crate::AgentErrorKind::OutcomeUnknown);
}

#[test]
fn local_composition_nested_scheduler_reserves_widths_two_and_sixteen() {
    for sequence in 0_u32..2 {
        let outstanding = usize::try_from(sequence).expect("bounded outstanding count");
        assert_eq!(
            crate::runtime::composition::nested::reserve_local_composition_nested_slot(
                outstanding,
                2,
                sequence
            )
            .expect("width-two reservation"),
            sequence + 1
        );
    }
    for sequence in 0_u32..16 {
        let outstanding = usize::try_from(sequence).expect("bounded outstanding count");
        assert_eq!(
            crate::runtime::composition::nested::reserve_local_composition_nested_slot(
                outstanding,
                16,
                sequence
            )
            .expect("width-sixteen reservation"),
            sequence + 1
        );
    }
}

#[test]
fn local_composition_nested_scheduler_denies_seventeenth_before_sequence_mint() {
    let error =
        crate::runtime::composition::nested::reserve_local_composition_nested_slot(16, 16, 16)
            .expect_err("the seventeenth outstanding call must be denied");
    assert_eq!(error.kind(), crate::AgentErrorKind::InvalidInput);
    assert!(error.message().contains("limit of 16"));
    assert_eq!(
        crate::runtime::composition::nested::reserve_local_composition_nested_slot(15, 16, 16)
            .expect("a freed slot remains usable"),
        17
    );
}

#[test]
fn local_composition_session_loss_maps_without_message_parsing() {
    use crate::local_composition::LocalCompositionSessionLoss;
    use crate::manifest::InterpreterResetReason;

    for (loss, expected) in [
        (
            LocalCompositionSessionLoss::HostLost("arbitrary detail".into()),
            InterpreterResetReason::HostLost,
        ),
        (
            LocalCompositionSessionLoss::ProtocolViolation("arbitrary detail".into()),
            InterpreterResetReason::ProtocolViolation,
        ),
        (
            LocalCompositionSessionLoss::Cancellation("arbitrary detail".into()),
            InterpreterResetReason::Cancellation,
        ),
        (
            LocalCompositionSessionLoss::AgentShutdown("arbitrary detail".into()),
            InterpreterResetReason::AgentShutdown,
        ),
    ] {
        assert_eq!(
            crate::runtime::composition::resources::local_composition_reset_reason(&loss),
            expected
        );
    }
}

#[test]
fn cleanup_result_aggregation_preserves_every_failure() {
    let result = crate::runtime::cleanup::combine_cleanup_results([
        crate::runtime::cleanup::combine_cleanup_results([
            Err(crate::AgentError::new(
                crate::AgentErrorKind::Model,
                "initial run failure",
            )),
            Err(crate::AgentError::new(
                crate::AgentErrorKind::Session,
                "local owner",
            )),
        ]),
        Err(crate::AgentError::new(
            crate::AgentErrorKind::Tool,
            "joined processes",
        )),
        Err(crate::AgentError::new(
            crate::AgentErrorKind::Internal,
            "integration",
        )),
        Err(crate::AgentError::new(
            crate::AgentErrorKind::Cancelled,
            "store flush",
        )),
    ])
    .expect_err("cleanup failures must remain visible");

    assert_eq!(result.kind(), crate::AgentErrorKind::OutcomeUnknown);
    for expected in [
        "initial run failure",
        "local owner",
        "joined processes",
        "integration",
        "store flush",
    ] {
        assert!(
            result.message().contains(expected),
            "cleanup diagnostic omitted {expected}: {}",
            result.message()
        );
    }
}

struct RecordingNestedResponder {
    identity: &'static str,
    deliveries: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl crate::local_composition::LocalCompositionNestedResponder for RecordingNestedResponder {
    fn resolve(
        self: Box<Self>,
        value: serde_json::Value,
    ) -> crate::local_composition::LocalCompositionFuture<
        'static,
        crate::local_composition::LocalCompositionNestedDelivery,
    > {
        Box::pin(async move {
            self.deliveries
                .lock()
                .expect("delivery lock")
                .push((self.identity.to_owned(), value.to_string()));
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }

    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> crate::local_composition::LocalCompositionFuture<
        'static,
        crate::local_composition::LocalCompositionNestedDelivery,
    > {
        Box::pin(async move {
            self.deliveries
                .lock()
                .expect("delivery lock")
                .push((self.identity.to_owned(), reason));
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }
}

#[tokio::test]
async fn local_composition_nested_results_drain_in_acceptance_order() {
    let deliveries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let responder = |identity| {
        Box::new(RecordingNestedResponder {
            identity,
            deliveries: std::sync::Arc::clone(&deliveries),
        }) as Box<dyn crate::local_composition::LocalCompositionNestedResponder>
    };
    let ready = |sequence, identity: &'static str| {
        let call = crate::local_composition::LocalCompositionNestedCall {
            cell_id: "cell".to_owned(),
            runtime_call_id: identity.to_owned(),
            tool_name: "read_file".to_owned(),
            input: None,
        };
        let digest =
            crate::runtime::composition::nested::local_composition_nested_request_digest(&call)
                .expect("request digest");
        crate::runtime::composition::LocalCompositionNestedReady::Rejected(Box::new((
            crate::runtime::composition::PendingLocalCompositionNested {
                call,
                responder: responder(identity),
                request_digest: digest,
                request_sequence: sequence,
            },
            crate::AgentError::new(
                crate::AgentErrorKind::InvalidInput,
                format!("sequence-{sequence}"),
            ),
        )))
    };
    let mut completed = std::collections::BTreeMap::new();
    let mut next = 1;
    completed.insert(2, ready(2, "sequence-2"));
    assert!(
        crate::runtime::composition::nested::take_next_local_composition_nested_ready(
            &mut completed,
            &mut next
        )
        .expect("buffer query")
        .is_none()
    );
    completed.insert(1, ready(1, "sequence-1"));
    while let Some(crate::runtime::composition::LocalCompositionNestedReady::Rejected(rejected)) =
        crate::runtime::composition::nested::take_next_local_composition_nested_ready(
            &mut completed,
            &mut next,
        )
        .expect("ordered drain")
    {
        let (pending, error) = *rejected;
        crate::runtime::composition::nested::deliver_local_composition_nested_results(
            pending.responder,
            Vec::new(),
            Err(error),
        )
        .await
        .expect("ordinary rejection delivery");
    }
    let recorded = deliveries.lock().expect("delivery lock").clone();
    assert_eq!(
        recorded,
        vec![
            ("sequence-1".to_owned(), "sequence-1".to_owned()),
            ("sequence-2".to_owned(), "sequence-2".to_owned()),
        ]
    );
}

#[tokio::test]
async fn local_composition_exact_duplicate_waiters_share_one_disposition() {
    let deliveries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let responder = |identity| {
        Box::new(RecordingNestedResponder {
            identity,
            deliveries: std::sync::Arc::clone(&deliveries),
        }) as Box<dyn crate::local_composition::LocalCompositionNestedResponder>
    };
    crate::runtime::composition::nested::deliver_local_composition_nested_results(
        responder("canonical"),
        vec![responder("duplicate-1"), responder("duplicate-2")],
        Ok(serde_json::json!({"value": 1})),
    )
    .await
    .expect("one resolved disposition fans out");
    assert_eq!(deliveries.lock().expect("delivery lock").len(), 3);

    deliveries.lock().expect("delivery lock").clear();
    let recovery =
        crate::AgentError::new(crate::AgentErrorKind::OutcomeUnknown, "recovery required");
    let returned = crate::runtime::composition::nested::deliver_local_composition_nested_results(
        responder("canonical"),
        vec![responder("duplicate")],
        Err(recovery.clone()),
    )
    .await
    .expect_err("recovery remains authoritative after fan-out");
    assert_eq!(returned, recovery);
    assert_eq!(deliveries.lock().expect("delivery lock").len(), 2);
}

#[test]
fn local_composition_nested_decision_is_typed_unsupported() {
    let tool =
        crate::runtime::composition::projection::local_composition_nested_decision_unsupported();
    assert_eq!(
        tool.kind(),
        plakous_tools::ToolRuntimeErrorKind::UnsupportedCapability
    );
    let error = crate::AgentError::from_tool(&tool);
    assert_eq!(error.kind(), crate::AgentErrorKind::Tool);
    assert_eq!(
        error.tool_kind(),
        Some(plakous_tools::ToolRuntimeErrorKind::UnsupportedCapability)
    );
}

#[tokio::test]
async fn local_composition_cancellation_fences_queued_calls_and_late_delivery_fails_closed() {
    let deliveries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let call = crate::local_composition::LocalCompositionNestedCall {
        cell_id: "cell".to_owned(),
        runtime_call_id: "runtime-1".to_owned(),
        tool_name: "read_file".to_owned(),
        input: Some(serde_json::json!({"path": "bounded"})),
    };
    let digest =
        crate::runtime::composition::nested::local_composition_nested_request_digest(&call)
            .expect("request digest");
    let mut pending = std::collections::VecDeque::from([
        crate::runtime::composition::PendingLocalCompositionNested {
            call: call.clone(),
            responder: Box::new(RecordingNestedResponder {
                identity: "runtime-1",
                deliveries: std::sync::Arc::clone(&deliveries),
            }),
            request_digest: digest,
            request_sequence: 1,
        },
    ]);
    let mut replays = std::collections::BTreeMap::from([(
        call.runtime_call_id.clone(),
        crate::runtime::composition::LocalCompositionNestedReplay {
            request_digest: digest,
            request_sequence: 1,
            disposition: crate::runtime::composition::LocalCompositionNestedDisposition::Pending,
            waiters: Vec::new(),
        },
    )]);
    let mut pending_deliveries = crate::runtime::composition::PendingNestedDeliveries::default();
    crate::runtime::composition::nested::fence_pending_local_composition_nested_queue(
        &mut pending,
        &mut replays,
        &mut pending_deliveries,
        "cell cancelled",
    );
    while let Some(delivery) = pending_deliveries.next().await {
        delivery.expect("fenced delivery");
    }
    assert_eq!(
        deliveries.lock().expect("delivery lock").as_slice(),
        &[("runtime-1".to_owned(), "cell cancelled".to_owned())]
    );
    let error = crate::runtime::composition::nested::local_composition_nested_replay(
        replays.get("runtime-1"),
        digest,
    )
    .expect_err("a fenced call cannot be resumed by a late result");
    assert_eq!(error.kind(), crate::AgentErrorKind::InvalidInput);
}

#[test]
fn retained_tool_exchange_failure_does_not_keep_a_stale_unshrunk_error() {
    use crate::{AgentError, AgentErrorKind};

    assert!(matches!(
        crate::runtime::observation::retained_tool_exchange_failure(Ok(())),
        Ok(None)
    ));
    let second = AgentError::new(AgentErrorKind::Internal, "shrunk admit failed");
    let Err(returned) = crate::runtime::observation::retained_tool_exchange_failure(Err(second))
    else {
        panic!("non-capacity second admit must surface as the turn error");
    };
    assert_eq!(returned.message(), "shrunk admit failed");
}

#[test]
fn clip_tool_result_keeps_small_text_and_replaces_overflow_with_budget_notice() {
    use crate::runtime::observation::budget::OBSERVATION_EXHAUSTED;
    use crate::runtime::observation::clip_tool_result;
    use plakous_model::{
        CallId, TextBlock, ToolCorrelation, ToolExecutionOwner, ToolResultBlock, ToolResultKind,
        ToolResultPart,
    };
    use uuid::Uuid;

    let call = CallId::try_from(Uuid::from_u128(7)).expect("call");
    let correlation = ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
        .expect("correlation");
    let small = ToolResultBlock::new(
        call,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new("seed-ok", None).expect("text"),
        )],
        correlation.clone(),
    )
    .expect("small result");
    let mut budget = crate::runtime::observation::budget::ObservationBudget::capped(Some(64));
    let (kept, kept_clip) = clip_tool_result(small, &mut budget).expect("clip small");
    assert_eq!(kept_clip, None, "a result that fits drops nothing");
    assert_eq!(kept.parts().len(), 1);
    assert_eq!(budget.remaining, Some(57));

    let large = ToolResultBlock::new(
        call,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new("x".repeat(80), None).expect("text"),
        )],
        correlation.clone(),
    )
    .expect("large result");
    let (clipped, large_clip) = clip_tool_result(large, &mut budget).expect("clip large");
    let large_clip = large_clip.expect("a truncated result reports what it dropped");
    assert!(large_clip.bytes_dropped > 0);
    assert!(large_clip.bound > 0);
    let ToolResultPart::Text(text) = &clipped.parts()[0] else {
        panic!("expected text");
    };
    assert!(
        text.text()
            .contains("bytes omitted from the middle of this output")
    );
    assert_eq!(budget.remaining, Some(0));

    let leftover = ToolResultBlock::new(
        call,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new("more", None).expect("text"),
        )],
        correlation,
    )
    .expect("leftover");
    let (exhausted, exhausted_clip) =
        clip_tool_result(leftover, &mut budget).expect("clip exhausted");
    assert!(
        exhausted_clip.is_some_and(|clip| clip.bytes_dropped > 0),
        "an exhausted budget drops the whole body and says so"
    );
    let ToolResultPart::Text(text) = &exhausted.parts()[0] else {
        panic!("expected text");
    };
    assert_eq!(text.text(), OBSERVATION_EXHAUSTED);
}

#[test]
fn clip_tool_result_preserves_process_session_handles() {
    use crate::runtime::observation::clip_tool_result;
    use plakous_model::{
        CallId, TextBlock, ToolCorrelation, ToolExecutionOwner, ToolResultBlock, ToolResultKind,
        ToolResultPart,
    };
    use uuid::Uuid;

    let call = CallId::try_from(Uuid::from_u128(9)).expect("call");
    let correlation = ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
        .expect("correlation");
    let body = serde_json::json!({
        "status": "running",
        "exit_code": null,
        "session_id": 12,
        "cursor": 4,
        "output": "x".repeat(400),
    })
    .to_string();
    let result = ToolResultBlock::new(
        call,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new(body, None).expect("text"),
        )],
        correlation,
    )
    .expect("result");
    let mut budget = crate::runtime::observation::budget::ObservationBudget::capped(Some(180));
    let (clipped, _) = clip_tool_result(result, &mut budget).expect("clip");
    let ToolResultPart::Text(text) = &clipped.parts()[0] else {
        panic!("expected text");
    };
    let value: serde_json::Value = serde_json::from_str(text.text()).expect("json");
    assert_eq!(value["session_id"], 12);
    assert_eq!(value["cursor"], 4);
    assert_eq!(value["status"], "running");
    assert!(
        value["output"]
            .as_str()
            .is_some_and(|output| output.contains("bytes omitted")
                || output.contains("withheld for context capacity"))
    );
}

#[test]
fn tool_subject_prefers_path_then_composition_source_and_code() {
    assert_eq!(
        crate::runtime::observation::tool_subject(&serde_json::json!({
            "path": "src/lib.rs",
            "source": "text('x')",
            "code": "print(1)"
        })),
        Some("src/lib.rs")
    );
    assert_eq!(
        crate::runtime::observation::tool_subject(
            &serde_json::json!({"source": "text(\"ptc-ok-js\")"})
        ),
        Some("text(\"ptc-ok-js\")")
    );
    assert_eq!(
        crate::runtime::observation::tool_subject(
            &serde_json::json!({"code": "print('ptc-ok-py')"})
        ),
        Some("print('ptc-ok-py')")
    );
}

/// The answer to a `wait` made while no cell is running is a success, and every
/// field of it has to say so: the model reads the kind, the ledger reads the
/// dispatch flag, and both must agree that nothing ran and nothing failed.
///
/// This replaces a `tool.rejected` record, which is
/// a gating condition for the tool-growth case kind.
#[test]
fn a_wait_with_no_active_cell_is_answered_as_an_empty_success() {
    use plakous_model::{ToolResultKind, ToolResultPart};

    let call = ToolCallBlock::new(
        "00000000-0000-7000-8000-000000000903"
            .parse::<CallId>()
            .expect("call ID"),
        ToolName::new("wait").expect("tool name"),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-wait".to_owned()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )
        .expect("correlation"),
        r#"{"cell_id":"none","yield_time_ms":1}"#.to_owned(),
    )
    .expect("tool call");

    let result = super::no_active_cell_wait_result(&call).expect("wait answer");
    assert_eq!(
        result.kind(),
        ToolResultKind::Success,
        "a question with an answer is not a failure"
    );
    assert_eq!(result.call_id(), call.call_id());
    assert_eq!(result.correlation(), call.correlation());
    let [ToolResultPart::Text(text)] = result.parts() else {
        panic!("the answer is one text part, got {:?}", result.parts());
    };
    assert_eq!(
        text.text(),
        "no local composition cell is active; there is nothing to wait for"
    );
}

/// C8: a process result's presentation preview stays JSON when its output is
/// far larger than the preview, so the exit code beside it and the output's
/// last lines still reach the host.
#[test]
fn a_large_structured_result_previews_as_bounded_json_with_its_small_fields() {
    use plakous_model::{TextBlock, ToolResultBlock, ToolResultKind, ToolResultPart};
    use uuid::Uuid;

    let call = CallId::try_from(Uuid::from_u128(10)).expect("call");
    let correlation = ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
        .expect("correlation");
    let output = format!(
        "{}error[E0425]: cannot find value `x`\n",
        "   Compiling crate v0.1.0\n".repeat(2_000)
    );
    let body = serde_json::json!({"exit_code": 101, "output": output}).to_string();
    let result = ToolResultBlock::new(
        call,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new(body, None).expect("text"),
        )],
        correlation,
    )
    .expect("result");
    let observation = crate::runtime::observation::tool_result_observation(&result);
    assert!(
        observation.preview().len() <= 2 * 1024,
        "{}",
        observation.preview().len()
    );
    let value: serde_json::Value =
        serde_json::from_str(observation.preview()).expect("the bounded preview is JSON");
    assert_eq!(value["exit_code"], 101);
    let output = value["output"].as_str().expect("output");
    assert!(output.contains("bytes omitted"), "{output}");
    assert!(
        output.ends_with("error[E0425]: cannot find value `x`\n"),
        "{output}"
    );
    assert_eq!(observation.subject(), None);
}

fn await_result(body: String) -> plakous_model::ToolResultBlock {
    use plakous_model::{TextBlock, ToolResultBlock, ToolResultKind, ToolResultPart};
    use uuid::Uuid;
    ToolResultBlock::new(
        CallId::try_from(Uuid::from_u128(12)).expect("call"),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new(body, None).expect("text"),
        )],
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
            .expect("correlation"),
    )
    .expect("result")
}

/// The 2 KiB preview of a topology envelope clips the receipt, so the children
/// cannot be read back out of it. The observation subject is the one the tool
/// supplied, not a parse of the receipt. A history read past 64 KiB keeps
/// that supplied subject too. A result that names `woke_by` has no subject
/// unless the tool set one.
#[test]
fn a_tool_supplied_subject_survives_a_clipped_preview() {
    let receipt = serde_json::json!({
        "agents": [
            {"agent_id": "c1", "output_json": "o".repeat(8 * 1024)},
            {"agent_id": "c2", "status": "running"}
        ],
        "mail": [],
        "timed_out": false,
        "woke_by": "settled",
    });
    let envelope = serde_json::json!({"result": receipt.to_string()}).to_string();
    let result = await_result(envelope);
    let sniffed = super::tool_result_observation(&result);
    assert_eq!(sniffed.subject(), None, "L0 does not parse a receipt");
    let observation = super::tool_result_observation_with_subject(&result, Some("c1,c2"));
    assert!(observation.preview().len() <= super::TOOL_OBSERVATION_PREVIEW_BYTES);
    let preview: serde_json::Value =
        serde_json::from_str(observation.preview()).expect("envelope preview stays JSON");
    let inner = preview["result"].as_str().expect("clipped receipt string");
    assert!(
        serde_json::from_str::<serde_json::Value>(inner).is_err(),
        "the clipped receipt must not still parse: {inner}"
    );
    assert_eq!(observation.subject(), Some("c1,c2"));
    let encoded = serde_json::to_value(&observation).expect("encode");
    assert_eq!(encoded["subject"], "c1,c2");
    let restored: crate::ToolResultObservation =
        serde_json::from_str(r#"{"succeeded":true,"preview":"x"}"#).expect("old event");
    assert_eq!(restored.subject(), None);

    let other =
        serde_json::json!({"exit_code": 0, "subject": "not-an-await", "woke_by": "settled"});
    assert_eq!(
        super::tool_result_observation(&await_result(other.to_string())).subject(),
        None
    );

    let huge = serde_json::json!({
        "agents": [{"agent_id": "c1", "output_json": "o".repeat(70 * 1024)}, {"agent_id": "c2"}],
        "mail": [],
        "timed_out": false,
        "woke_by": "settled",
    });
    let huge = serde_json::json!({"result": huge.to_string()}).to_string();
    assert!(huge.len() > 64 * 1024, "{}", huge.len());
    let history = super::history_tool_result_observation_with_subject(
        &await_result(huge.clone()),
        Some("c1,c2"),
    );
    assert_eq!(history.subject(), Some("c1,c2"));
    assert_eq!(
        super::history_tool_result_observation_with_subject(&await_result(huge), None).subject(),
        None
    );
}

#[test]
fn observation_stops_at_a_complete_part_before_redacting_and_bounding() {
    use plakous_model::{TextBlock, ToolResultBlock, ToolResultKind, ToolResultPart};
    use uuid::Uuid;

    let body = serde_json::json!({"output": "x".repeat(2 * 1024), "api_key": "short"}).to_string();
    let result = ToolResultBlock::new(
        CallId::try_from(Uuid::from_u128(11)).expect("call"),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![
            ToolResultPart::Text(TextBlock::new(body.clone(), None).expect("text")),
            ToolResultPart::Text(TextBlock::new("later part", None).expect("text")),
        ],
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
            .expect("correlation"),
    )
    .expect("result");
    let observation = super::tool_result_observation(&result);
    assert!(observation.preview().len() <= super::TOOL_OBSERVATION_PREVIEW_BYTES);
    let value: serde_json::Value =
        serde_json::from_str(observation.preview()).expect("complete first part stays JSON");
    assert_eq!(value["api_key"], "[redacted]");
    assert!(!observation.preview().contains("later part"));
    let ToolResultPart::Text(first) = &result.parts()[0] else {
        panic!("text part");
    };
    assert_eq!(first.text(), body);
}

fn batch_item(index: u128, body: &str) -> plakous_model::ConversationItem {
    use plakous_model::{
        Actor, ContentBlock, ConversationItem, EntryId, Provenance, TextBlock, ToolResultBlock,
        ToolResultKind, ToolResultPart,
    };
    use uuid::Uuid;
    let result = ToolResultBlock::new(
        CallId::try_from(Uuid::from_u128(100 + index)).expect("call"),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new(body, None).expect("text"),
        )],
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
            .expect("correlation"),
    )
    .expect("result");
    ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(200 + index)).expect("entry"),
        Actor::Tool,
        vec![ContentBlock::ToolResult(result)],
        Provenance::ToolExecution,
    )
    .expect("item")
}

fn batch_text(item: &plakous_model::ConversationItem) -> &str {
    use plakous_model::{ContentBlock, ToolResultPart};
    let ContentBlock::ToolResult(result) = &item.blocks()[0] else {
        panic!("tool result");
    };
    let ToolResultPart::Text(text) = &result.parts()[0] else {
        panic!("text part");
    };
    text.text()
}

/// One huge build log beside four small results overflows the window. The
/// small results are what the model asked for too; they must survive whole,
/// and the log keeps its failing tail.
#[test]
fn an_overflowing_batch_keeps_small_results_whole_and_clips_the_large_one() {
    use crate::runtime::observation::{
        shrink_tool_exchange_observations, tool_exchange_shrink_budget,
    };
    let mut log = String::new();
    for index in 0..8_000 {
        writeln!(log, "test module_{index}::case ... ok").expect("write to String");
    }
    log.push_str("test parser::rejects_trailing_comma ... FAILED\n");
    let small: Vec<String> = (0..4)
        .map(|index| format!("small-result-{index} {}", "y".repeat(1_500)))
        .collect();
    let mut items = vec![batch_item(0, &log)];
    items.extend(
        small
            .iter()
            .enumerate()
            .map(|(index, body)| batch_item(index as u128 + 1, body)),
    );
    let total: u64 = (log.len() + small.iter().map(String::len).sum::<usize>()) as u64;
    // The request is over by half of the batch's one-token-per-byte bound.
    let overflow: plakous_model::ContextOverflow = serde_json::from_value(serde_json::json!({
        "shared": {"required": 100_000 + total / 2, "available": 100_000},
        "input": null,
        "output": null,
    }))
    .expect("overflow");
    let budget = tool_exchange_shrink_budget(&items, overflow).expect("a budget remains");
    let (shrunk, replacements) =
        shrink_tool_exchange_observations(items, Some(budget)).expect("shrink");
    for (index, body) in small.iter().enumerate() {
        assert_eq!(batch_text(&shrunk[index + 1]), body, "small result {index}");
    }
    let clipped = batch_text(&shrunk[0]);
    assert!(clipped.len() < log.len() / 2);
    assert!(clipped.contains("rejects_trailing_comma ... FAILED"));
    assert_eq!(replacements.len(), 1, "only the log was replaced");
    let kept: usize = shrunk.iter().map(|item| batch_text(item).len()).sum();
    assert!(kept <= budget + 256, "kept {kept} of budget {budget}");
}

/// Without a budget, or when the fair share falls under the floor, a large
/// result gives its space up entirely rather than keep a useless sliver.
#[test]
fn a_share_under_the_floor_exhausts_the_large_results() {
    use crate::runtime::observation::budget::OBSERVATION_EXHAUSTED;
    use crate::runtime::observation::shrink_tool_exchange_observations;
    let items = vec![
        batch_item(0, &"a".repeat(50_000)),
        batch_item(1, &"b".repeat(50_000)),
        batch_item(2, "tiny"),
    ];
    let (shrunk, _) =
        shrink_tool_exchange_observations(items.clone(), Some(3_000)).expect("shrink");
    assert_eq!(batch_text(&shrunk[0]), OBSERVATION_EXHAUSTED);
    assert_eq!(batch_text(&shrunk[1]), OBSERVATION_EXHAUSTED);
    assert_eq!(batch_text(&shrunk[2]), "tiny");
    let (shrunk, _) = shrink_tool_exchange_observations(items, None).expect("shrink");
    assert_eq!(batch_text(&shrunk[2]), "tiny");
}

fn history_call(arguments: &serde_json::Value) -> ToolCallBlock {
    ToolCallBlock::new(
        "00000000-0000-7000-8000-000000000905"
            .parse::<CallId>()
            .expect("call ID"),
        ToolName::new("exec_command").expect("name"),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
            .expect("correlation"),
        arguments.to_string(),
    )
    .expect("call")
}

fn history_result(text: &str) -> plakous_model::ToolResultBlock {
    plakous_model::ToolResultBlock::new(
        "00000000-0000-7000-8000-000000000905"
            .parse::<CallId>()
            .expect("call ID"),
        ToolExecutionOwner::Local,
        plakous_model::ToolResultKind::Success,
        vec![plakous_model::ToolResultPart::Text(
            plakous_model::TextBlock::new(text, None).expect("text"),
        )],
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
            .expect("correlation"),
    )
    .expect("result")
}

const HISTORY_SECRET: &str = "sk-abcdefghijklmnopqrstuvwxyz0123456789";

/// Text of `bytes` filler with `secret` placed to straddle byte `at`.
fn filler_with_secret_at(bytes: usize, at: usize) -> String {
    let mut text = "line of output\n".repeat(bytes / 15 + 1);
    text.truncate(bytes);
    let secret = format!(" {HISTORY_SECRET} ");
    let start = at.saturating_sub(secret.len() / 2);
    text.replace_range(start..start + secret.len(), &secret);
    text
}

/// A history read observes a call and a result exactly as the live event
/// did within the exact bound, including JSON results and credentials near
/// the window edge; past it, the read stays bounded and redacted and keeps
/// the live preview's head.
#[test]
fn history_observations_match_live_and_window_large_sources() {
    let window = super::HISTORY_WINDOW_BYTES;
    let exact = super::HISTORY_EXACT_BYTES;
    let texts = [
        format!("short {HISTORY_SECRET} result"),
        filler_with_secret_at(window + 512, window),
        filler_with_secret_at(exact - 64, 100),
        serde_json::json!({
            "exit_code": 0,
            "output": filler_with_secret_at(40 * 1024, window),
        })
        .to_string(),
        filler_with_secret_at(exact * 4, 100),
    ];
    for text in &texts {
        let result = history_result(text);
        let live = super::tool_result_observation(&result);
        let history = super::history_tool_result_observation_with_subject(&result, None);
        assert_eq!(history.succeeded(), live.succeeded());
        assert_eq!(history.preview(), live.preview(), "{} bytes", text.len());
        assert!(!history.preview().contains(HISTORY_SECRET));
        let call = history_call(&serde_json::json!({"cmd": "cat", "input": text}));
        let live = super::tool_call_observation(&call);
        let history = super::history_tool_call_observation(&call);
        assert_eq!(history.subject(), live.subject(), "{} bytes", text.len());
        assert_eq!(
            history.arguments_preview(),
            live.arguments_preview(),
            "{} bytes",
            text.len()
        );
        assert!(!history.arguments_preview().contains(HISTORY_SECRET));
    }
    // A credential cut by the window shrinks the redacted window, so the
    // read may show less than live, but only a prefix of it, never the cut.
    for at in [window, window - 4] {
        let text = filler_with_secret_at(exact * 4, at);
        let result = history_result(&text);
        let live = super::tool_result_observation(&result);
        let history = super::history_tool_result_observation_with_subject(&result, None);
        let shown = history.preview().trim_end_matches("\n[preview truncated]");
        assert!(live.preview().starts_with(shown), "{at}");
        assert!(history.preview().len() <= super::TOOL_OBSERVATION_PREVIEW_BYTES);
        assert!(!history.preview().contains("sk-"));
        let call = history_call(&serde_json::json!({"cmd": "cat", "input": text}));
        let history = super::history_tool_call_observation(&call);
        assert!(!history.arguments_preview().contains("sk-"));
        assert!(history.arguments_preview().len() <= super::TOOL_OBSERVATION_PREVIEW_BYTES);
    }
    // A large JSON result past the exact bound keeps its structured preview
    // and the key rule: every leaf under a credential-named key is redacted,
    // however short, wherever the large leaf sits.
    for large in [
        serde_json::json!({
            "exit_code": 0,
            "output": "x".repeat(exact * 2),
            "auth": {"api_key": "short-key-1"},
        }),
        serde_json::json!([{"password": "short-key-1"}, "x".repeat(exact * 2)]),
    ] {
        let text = large.to_string();
        let live = super::tool_result_observation(&history_result(&text));
        assert!(
            !live.preview().contains("short-key-1"),
            "{}",
            live.preview()
        );
        let history =
            super::history_tool_result_observation_with_subject(&history_result(&text), None);
        assert!(history.preview().len() <= super::TOOL_OBSERVATION_PREVIEW_BYTES);
        assert!(
            !history.preview().contains("short-key-1"),
            "{}",
            history.preview()
        );
        let structured = serde_json::from_str::<serde_json::Value>(history.preview())
            .expect("structured history preview");
        if let Some(exit_code) = structured.get("exit_code") {
            assert_eq!(exit_code, 0);
        }
    }
    // A large text that only looks like JSON is still read as bounded text.
    let broken = format!("{{\"output\": \"{}", "x".repeat(exact * 2));
    let history =
        super::history_tool_result_observation_with_subject(&history_result(&broken), None);
    assert!(history.preview().len() <= super::TOOL_OBSERVATION_PREVIEW_BYTES);
    assert!(history.preview().ends_with("[preview truncated]"));
    // Redaction that shrinks the window never pulls its cut edge into view.
    let dense = format!("{HISTORY_SECRET} ").repeat(exact / 20);
    let history =
        super::history_tool_result_observation_with_subject(&history_result(&dense), None);
    assert!(!history.preview().contains("sk-"), "{}", history.preview());
}

/// A JSON result past the exact bound keeps the JSON key rule (spec 04 §5)
/// in the history read: a short credential under a credential-named key
/// that the live preview redacts is not shown in the agent view.
#[test]
fn large_json_result_keeps_the_key_redaction_live_applies() {
    let text = format!(
        "{{\"password\":\"hunter2\",\"output\":\"{}\"}}",
        "x".repeat(super::HISTORY_EXACT_BYTES + 1024)
    );
    let result = history_result(&text);
    let live = super::tool_result_observation(&result);
    assert!(!live.preview().contains("hunter2"), "{}", live.preview());
    let history = super::history_tool_result_observation_with_subject(&result, None);
    assert!(
        !history.preview().contains("hunter2"),
        "history shows what live redacts: {}",
        &history.preview()[..120]
    );
}

/// A process result past the exact bound keeps its structured preview in
/// the history read, so the host's exit-code facts (`process_failure`, wait
/// facts) read it and a failed command reads `failed:exit 1` in the agent
/// view as its live row does.
#[test]
fn large_json_process_result_keeps_its_exit_code() {
    // 40,000 output bytes (the process observation cap) of colored output
    // JSON-escape past 64 KiB.
    let output = "\u{1b}[31merr\u{1b}[0m\n".repeat(40_000 / 13);
    let text = serde_json::json!({"exit_code": 1, "output": output}).to_string();
    assert!(text.len() > super::HISTORY_EXACT_BYTES, "{}", text.len());
    let result = history_result(&text);
    let exit_code = |preview: &str| {
        serde_json::from_str::<serde_json::Value>(preview.trim())
            .ok()
            .and_then(|value| value.get("exit_code").and_then(serde_json::Value::as_i64))
    };
    let live = super::tool_result_observation(&result);
    assert_eq!(exit_code(live.preview()), Some(1), "{}", live.preview());
    let history = super::history_tool_result_observation_with_subject(&result, None);
    assert_eq!(
        exit_code(history.preview()),
        Some(1),
        "history preview is not structured: {}",
        &history.preview()[..120]
    );
}

/// A credential-named key longer than the history window is cut by the key
/// window, so the JSON key rule is decided on the full key: the short value
/// under it stays redacted, as live redacts it.
#[test]
fn long_credential_key_keeps_its_value_redacted() {
    let key = format!(
        "sk-{}_password",
        "A".repeat(super::HISTORY_WINDOW_BYTES + 4096)
    );
    let text = format!(
        "{{\"{key}\":\"hunter22\",\"output\":\"{}\"}}",
        "x".repeat(super::HISTORY_EXACT_BYTES)
    );
    let result = history_result(&text);
    let live = super::tool_result_observation(&result);
    assert!(!live.preview().contains("hunter22"), "{}", live.preview());
    let history = super::history_tool_result_observation_with_subject(&result, None);
    assert!(history.preview().len() <= super::TOOL_OBSERVATION_PREVIEW_BYTES);
    assert!(
        !history.preview().contains("hunter22"),
        "history shows what live redacts: {}",
        history.preview()
    );
}

/// The skipped tail is parsed as strictly as the live `Value` parse (a lone
/// surrogate, nesting past `serde_json`'s 128 levels), so history reads such a
/// document as text, as live does, and a value the text rules hide but the
/// key rule does not cover stays hidden.
#[test]
fn history_parse_is_as_strict_as_live() {
    let head = format!(
        "{{\"access token\":\"hunter22xx\",\"p1\":\"{pad}\",\"p2\":\"{pad}\"",
        pad = "x".repeat(19_000)
    );
    let fill = "y".repeat(super::HISTORY_EXACT_BYTES / 2);
    let surrogate = format!("{head},\"tail\":\"\\ud800{fill}\"}}");
    let deep = format!(
        "{head},\"tail\":[\"{fill}\",{}{}]}}",
        "[".repeat(200),
        "]".repeat(200)
    );
    for text in [surrogate, deep] {
        assert!(text.len() > super::HISTORY_EXACT_BYTES);
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_err());
        let result = history_result(&text);
        let live = super::tool_result_observation(&result);
        assert!(!live.preview().contains("hunter22xx"), "{}", live.preview());
        let history = super::history_tool_result_observation_with_subject(&result, None);
        assert!(
            !history.preview().contains("hunter22xx"),
            "history shows what live redacts: {}",
            &history.preview()[..200]
        );
    }
}

/// Empty arrays and objects spend budget, so the kept value is bounded by
/// the window, not by the document.
#[test]
fn windowed_json_bounds_empty_containers() {
    for text in [
        format!("[{}[]]", "[],".repeat(100_000)),
        format!("[{}{{}}]", "{},".repeat(100_000)),
    ] {
        let value = super::history_json::windowed_json(serde_json::Deserializer::from_str(&text))
            .expect("json");
        let kept = value.as_array().map_or(0, Vec::len);
        assert!(
            kept <= super::HISTORY_WINDOW_BYTES,
            "{kept} empty containers kept"
        );
    }
}

/// Hostile large JSON never panics,
/// never overflows the stack and always previews within the bound.
#[test]
fn hostile_large_json_stays_bounded() {
    let exact = super::HISTORY_EXACT_BYTES;
    let pad = "x".repeat(exact);
    let texts = [
        format!("{}{}", "[".repeat(exact + 10), "]".repeat(exact + 10)),
        format!("[\"{pad}\"]] trailing"),
        format!("{{\"a\":\"{pad}\",\"b\":nope}}"),
        format!("[{}]", "9".repeat(exact + 10)),
        format!("[1e999999,\"{pad}\"]"),
        format!("[\"\\ud800{pad}\"]"),
        format!("{{\"password\":[1,true,null,{{\"x\":2.5}}],\"o\":\"{pad}\"}}"),
        format!("[{}\"{pad}\"]", "{\"k\":1},".repeat(exact / 8)),
    ];
    for text in &texts {
        let history =
            super::history_tool_result_observation_with_subject(&history_result(text), None);
        assert!(history.preview().len() <= super::TOOL_OBSERVATION_PREVIEW_BYTES);
        assert!(!history.preview().contains("2.5"), "{}", history.preview());
    }
}
