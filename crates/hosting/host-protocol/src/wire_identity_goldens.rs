//! Identity goldens for the v1 JSONL wire.
//!
//! `round_trip_tests` proves each type matches a `json!` literal; this module
//! pins the *bytes* an external client actually reads, including the key order
//! [`crate::envelope::encode_object`] emits and the exact absence of every
//! `skip_serializing_if` field. v1 is frozen and grows additively only, so a
//! changed line here is a new major, never a refactor.
//!
//! Re-pin deliberately with
//! `PLAK_PRINT_WIRE_PINS=1 cargo test -p plakous-host-protocol wire_identity -- --nocapture`.

use serde::Serialize;
use serde_json::json;

use crate::envelope::{
    MAX_PROMPT_BYTES, REQUIRED_STDOUT_KINDS, SCHEMA_V, bye_line, diagnostic_line, encode_object,
    hello_line, ready_line,
};
use crate::topology::{
    AdoptCandidateRequest, AdoptOutcome, AttemptId, AttemptScopedRequest, CandidateId, Delegation,
    ListAgentsRequest, MessagePriority, MessagePurpose, SendAgentMessageRequest, SpawnAgentRequest,
    SpawnWorkspace, SubmitResultRequest, TOPOLOGY_PRESENTATION_ORDER, TemplateName, Tier,
    TopologyErrorCode, WaitAgentRequest, WorkspaceIdRequest,
};

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
///
/// The exact bytes of every outbound line whose shape is fixed by the
/// handshake rather than by a host observation, plus the refusal line. Keys
/// come out lexicographically ordered because `serde_json::Map` is a sorted
/// map; a client reads by name, but every recorded fixture holds this order.
const HANDSHAKE_LINE_PINS: &[(&str, &str)] = &[
    (
        "hello/full",
        r#"{"account":"codex","composition":"direct","effort":"low","elapsed_ms":12,"id":"c1","javascript_host":"sidecar","javascript_host_target":"plakous-js-host","kind":"hello","model":"gpt-5.6-sol","python_host":"sidecar","python_host_target":"plak-py-host","session_id":"00000000-0000-4000-8000-000000000001","tree_id":"tree-1","tree_state":"live","v":1}"#,
    ),
    (
        "hello/minimal",
        r#"{"composition":"plakous-default","kind":"hello","session_id":"00000000-0000-4000-8000-000000000001","tree_id":"tree-1","tree_state":"live","v":1}"#,
    ),
    (
        "ready/full",
        r#"{"elapsed_ms":7,"id":"c1","kind":"ready","v":1}"#,
    ),
    ("ready/minimal", r#"{"kind":"ready","v":1}"#),
    ("bye/full", r#"{"id":"c1","kind":"bye","v":1}"#),
    ("bye/minimal", r#"{"kind":"bye","v":1}"#),
    (
        "error/full",
        r#"{"id":"c1","kind":"error","message":"unknown kind","v":1}"#,
    ),
    (
        "error/minimal",
        r#"{"kind":"error","message":"unknown kind","v":1}"#,
    ),
    (
        "encode_object/field-order",
        r#"{"alpha":1,"id":"c1","kind":"tool","v":1,"zulu":2}"#,
    ),
];

#[test]
fn wire_identity_background_and_job_events_are_pinned() {
    use crate::envelope::JsonlCorrelation;
    use crate::jsonl::project_host_event;
    use crate::{Core, SessionObservation};

    let background = encode_object(
        serde_json::from_value(json!({"v":1,"kind":"background","id":"b1"})).unwrap(),
    );
    assert_eq!(background, r#"{"id":"b1","kind":"background","v":1}"#);
    let events: [(SessionObservation<Core>, &str); 3] = [
        (
            SessionObservation::JobStarted {
                job_id: "p1".into(),
                owner: "root".into(),
                command: "read line".into(),
            },
            r#"{"command":"read line","job_id":"p1","kind":"job_started","owner":"root","v":1}"#,
        ),
        (
            SessionObservation::JobPromoted {
                job_id: "p1".into(),
                owner: "root".into(),
            },
            r#"{"job_id":"p1","kind":"job_promoted","owner":"root","v":1}"#,
        ),
        (
            SessionObservation::JobTerminal {
                job_id: "p1".into(),
                owner: "root".into(),
                status: "exited".into(),
                exit_code: Some(0),
                stop_reason: None,
                elapsed_ms: 250,
                output: "done".into(),
            },
            r#"{"elapsed_ms":250,"exit_code":0,"job_id":"p1","kind":"job_terminal","output":"done","owner":"root","status":"exited","v":1}"#,
        ),
    ];
    for (event, expected) in events {
        let correlation = JsonlCorrelation {
            prompt_id: Some("unrelated"),
            command_id: Some("b1"),
            ..JsonlCorrelation::default()
        };
        assert_eq!(
            project_host_event(&event, correlation).as_deref(),
            Some(expected)
        );
    }
}

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
///
/// The exact serialization of one value of every topology command payload and
/// receipt. These types move crate under the re-layering program; the bytes
/// must not.
const TOPOLOGY_PAYLOAD_PINS: &[(&str, &str)] = &[
    (
        "spawn_agent/full",
        r#"{"template":"review","description":"reviewer","prompt":"review the diff","output_schema_json":"{}","tier":"read_only","inputs":["cand-a2-1"],"request_key":"k1"}"#,
    ),
    (
        "spawn_agent/minimal",
        r#"{"template":"review","description":"reviewer","prompt":"review the diff","output_schema_json":"{}","tier":"mutate"}"#,
    ),
    (
        "send_agent_message/full",
        r#"{"agent_id":"a2","message":"follow up","purpose":"reply","priority":"urgent","in_reply_to":"m1","output_schema_json":"{}"}"#,
    ),
    (
        "list_agents/full",
        r#"{"include_self":true,"include_completed":true}"#,
    ),
    (
        "list_agents/minimal",
        r#"{"include_self":null,"include_completed":null}"#,
    ),
    (
        "wait_agent/full",
        r#"{"agent_ids":["a2"],"timeout_ms":1000}"#,
    ),
    ("wait_agent/minimal", r#"{"timeout_ms":null}"#),
    (
        "attempt_scoped/full",
        r#"{"agent_id":"a2","expected_attempt_id":"a2-a1"}"#,
    ),
    ("attempt_scoped/minimal", r#"{"agent_id":"a2"}"#),
    (
        "submit_result",
        r#"{"turn_token":1,"output_json":"{\"ok\":true}"}"#,
    ),
    ("workspace_id", r#"{"workspace_id":"w1"}"#),
    (
        "adopt_candidate/full",
        r#"{"candidate_id":"cand-a2-1","expected_target":"0000000000000000000000000000000000000000","evidence":["a3-1"]}"#,
    ),
    ("adopt_candidate/minimal", r#"{"candidate_id":"cand-a2-1"}"#),
    (
        "adopt_outcome",
        r#"{"candidate_id":"cand-a2-1","target_before":"0000000000000000000000000000000000000000","target_after":"1111111111111111111111111111111111111111","files_changed":3}"#,
    ),
];

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
///
/// The closed refusal vocabulary and the closed enum spellings a client
/// branches on. Adding a variant is additive; changing one of these strings is
/// a new major.
const CLOSED_VOCABULARY_PINS: &[(&str, &str)] = &[
    ("schema_v", "1"),
    ("max_prompt_bytes", "65536"),
    (
        "required_stdout_kinds",
        "hello,ready,tool,error,turn,turn_lifecycle,tree,bye,model_refusal,recovered,retry",
    ),
    (
        "invoke_tool_names",
        "spawn_agent,list_agents,send_agent_message,wait_agent,interrupt_agent,close_agent,recover_agent,submit_result,adopt_candidate,discard_workspace",
    ),
    ("tier", "read_only,execute,mutate"),
    ("delegation", "explicit,proactive"),
    (
        "message_purpose",
        "coordinate,finding,question,reply,followup,result",
    ),
    ("message_priority", "deferred,urgent"),
    (
        "topology_error_codes",
        "unknown_field,schema_mismatch,bound_exceeded,grant_denied,self_send,recipient_closed,recipient_attempt_terminal,recipient_recovery_required,reply_invalid,mailbox_full,spawn_budget,capacity,request_key_conflict,depth_exhausted,template_not_spawnable,template_unknown,mint_unavailable,mint_hook_failed,input_not_sealed,workspace_identity_mismatch,workspace_not_owned,workspace_referenced,model_not_admitted,submit_root_forbidden,submit_stale_token,submit_duplicate_conflict,live_work,tree_restored_paused,operation_recovery_required,candidate_unknown,not_sealed,not_authorized,evidence_mismatch,target_changed,merge_conflict,adopt_not_applicable,discard_not_applicable,followup_not_idle,quota_blocked,attempt_mismatch,no_pending_children,wait_caller_settled,recovery_lease_held,recovery_owner_live,recovery_storage_unavailable,recovery_effect_evidence_refused,recovery_vcs_ambiguous,recovery_session_unavailable",
    ),
];

fn print_pins_requested() -> bool {
    std::env::var("PLAK_PRINT_WIRE_PINS").is_ok_and(|value| value == "1")
}

fn check(
    failures: &mut Vec<String>,
    rows: &mut Vec<String>,
    pins: &[(&str, &str)],
    case: &str,
    actual: &str,
) {
    rows.push(format!("    (\"{case}\", r#\"{actual}\"#),"));
    match pins.iter().find(|(name, _)| *name == case) {
        None => failures.push(format!("{case}: case has no pinned line")),
        Some((_, pinned)) if *pinned == actual => {}
        Some((_, pinned)) => failures.push(format!(
            "{case}: wire bytes changed\n  pinned {pinned}\n  actual {actual}"
        )),
    }
}

fn report(kind: &str, table: &str, rows: &[String], failures: &[String]) {
    if print_pins_requested() {
        println!("{table}:\n{}", rows.join("\n"));
    }
    assert!(
        failures.is_empty(),
        "{kind} drift; a change here is a wire/durable-format change and needs the maintainers' decision (re-pin with PLAK_PRINT_WIRE_PINS=1 cargo test -p plakous-host-protocol wire_identity -- --nocapture):\n\n{}",
        failures.join("\n\n")
    );
}

fn encoded<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("topology payload is serializable")
}

fn template(name: &str) -> TemplateName {
    TemplateName::parse(name).expect("literal template name")
}

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive pin table; splitting it hides what is covered"
)]
fn wire_identity_handshake_lines_are_pinned() {
    let mut rows = Vec::new();
    let mut failures = Vec::new();

    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "hello/full",
        &hello_line(
            Some("c1"),
            "00000000-0000-4000-8000-000000000001",
            "tree-1",
            Some("codex"),
            Some("gpt-5.6-sol"),
            Some("low"),
            "direct",
            "live",
            Some("sidecar"),
            Some("plakous-js-host"),
            Some("sidecar"),
            Some("plak-py-host"),
            Some(12),
        ),
    );
    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "hello/minimal",
        &hello_line(
            None,
            "00000000-0000-4000-8000-000000000001",
            "tree-1",
            None,
            None,
            None,
            "plakous-default",
            "live",
            None,
            None,
            None,
            None,
            None,
        ),
    );
    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "ready/full",
        &ready_line(Some("c1"), Some(7)),
    );
    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "ready/minimal",
        &ready_line(None, None),
    );
    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "bye/full",
        &bye_line(Some("c1")),
    );
    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "bye/minimal",
        &bye_line(None),
    );
    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "error/full",
        &diagnostic_line(Some("c1"), "unknown kind"),
    );
    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "error/minimal",
        // An empty id is dropped rather than written as `""`.
        &diagnostic_line(Some(""), "unknown kind"),
    );

    // `encode_object` moves `kind` and `id` to the front of its own map, but
    // `serde_json::Map` is sorted, so the emitted key order is lexicographic
    // regardless of insertion order. Pinned because recorded fixtures hold it.
    let mut fields = serde_json::Map::new();
    fields.insert("zulu".to_owned(), json!(2));
    fields.insert("alpha".to_owned(), json!(1));
    fields.insert("id".to_owned(), json!("c1"));
    fields.insert("kind".to_owned(), json!("tool"));
    check(
        &mut failures,
        &mut rows,
        HANDSHAKE_LINE_PINS,
        "encode_object/field-order",
        &encode_object(fields),
    );

    report("handshake line", "HANDSHAKE_LINE_PINS", &rows, &failures);
}

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive pin table; splitting it hides what is covered"
)]
fn wire_identity_topology_payloads_are_pinned() {
    let mut rows = Vec::new();
    let mut failures = Vec::new();

    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "spawn_agent/full",
        &encoded(&SpawnAgentRequest {
            template: template("review"),
            description: "reviewer".into(),
            prompt: "review the diff".into(),
            output_schema_json: "{}".into(),
            tier: Tier::ReadOnly,
            workspace: SpawnWorkspace::Inherit,
            inputs: Some(vec!["cand-a2-1".into()]),
            request_key: Some("k1".into()),
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "spawn_agent/minimal",
        &encoded(&SpawnAgentRequest {
            template: template("review"),
            description: "reviewer".into(),
            prompt: "review the diff".into(),
            output_schema_json: "{}".into(),
            tier: Tier::Mutate,
            workspace: SpawnWorkspace::Inherit,
            inputs: None,
            request_key: None,
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "send_agent_message/full",
        &encoded(&SendAgentMessageRequest {
            agent_id: "a2".into(),
            message: "follow up".into(),
            purpose: Some(MessagePurpose::Reply),
            priority: Some(MessagePriority::Urgent),
            in_reply_to: Some("m1".into()),
            output_schema_json: Some("{}".into()),
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "list_agents/full",
        &encoded(&ListAgentsRequest {
            include_read: None,
            include_self: Some(true),
            include_completed: Some(true),
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "list_agents/minimal",
        &encoded(&ListAgentsRequest {
            include_read: None,
            include_self: None,
            include_completed: None,
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "wait_agent/full",
        &encoded(&WaitAgentRequest {
            agent_ids: Some(vec!["a2".into()]),
            timeout_ms: Some(1_000),
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "wait_agent/minimal",
        &encoded(&WaitAgentRequest {
            agent_ids: None,
            timeout_ms: None,
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "attempt_scoped/full",
        &encoded(&AttemptScopedRequest {
            agent_id: "a2".into(),
            expected_attempt_id: Some(AttemptId::new("a2", 1)),
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "attempt_scoped/minimal",
        &encoded(&AttemptScopedRequest {
            agent_id: "a2".into(),
            expected_attempt_id: None,
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "submit_result",
        &encoded(&SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "workspace_id",
        &encoded(&WorkspaceIdRequest {
            workspace_id: "w1".into(),
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "adopt_candidate/full",
        &encoded(&AdoptCandidateRequest {
            candidate_id: CandidateId::from_persisted("cand-a2-1".to_owned()),
            expected_target: Some("0".repeat(40)),
            evidence: Some(vec!["a3-1".into()]),
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "adopt_candidate/minimal",
        &encoded(&AdoptCandidateRequest {
            candidate_id: CandidateId::from_persisted("cand-a2-1".to_owned()),
            expected_target: None,
            evidence: None,
        }),
    );
    check(
        &mut failures,
        &mut rows,
        TOPOLOGY_PAYLOAD_PINS,
        "adopt_outcome",
        &encoded(&AdoptOutcome {
            candidate_id: CandidateId::from_persisted("cand-a2-1".to_owned()),
            target_before: "0".repeat(40),
            target_after: "1".repeat(40),
            files_changed: 3,
        }),
    );

    report(
        "topology payload",
        "TOPOLOGY_PAYLOAD_PINS",
        &rows,
        &failures,
    );
}

fn joined<T: Serialize>(values: &[T]) -> String {
    values
        .iter()
        .map(|value| {
            serde_json::to_value(value)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .expect("closed vocabulary member serializes as a string")
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive pin table; splitting it hides what is covered"
)]
fn wire_identity_closed_vocabularies_are_pinned() {
    let mut rows = Vec::new();
    let mut failures = Vec::new();

    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "schema_v",
        &SCHEMA_V.to_string(),
    );
    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "max_prompt_bytes",
        &MAX_PROMPT_BYTES.to_string(),
    );
    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "required_stdout_kinds",
        &REQUIRED_STDOUT_KINDS.join(","),
    );
    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "invoke_tool_names",
        &TOPOLOGY_PRESENTATION_ORDER.join(","),
    );
    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "tier",
        &joined(&[Tier::ReadOnly, Tier::Execute, Tier::Mutate]),
    );
    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "delegation",
        &joined(&[Delegation::Explicit, Delegation::Proactive]),
    );
    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "message_purpose",
        &joined(&[
            MessagePurpose::Coordinate,
            MessagePurpose::Finding,
            MessagePurpose::Question,
            MessagePurpose::Reply,
            MessagePurpose::Followup,
            MessagePurpose::Result,
        ]),
    );
    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "message_priority",
        &joined(&[MessagePriority::Deferred, MessagePriority::Urgent]),
    );

    // The refusal vocabulary is a closed set whose `snake_case` spellings a
    // client branches on; every variant is listed so adding one is deliberate.
    check(
        &mut failures,
        &mut rows,
        CLOSED_VOCABULARY_PINS,
        "topology_error_codes",
        &joined(&[
            TopologyErrorCode::UnknownField,
            TopologyErrorCode::SchemaMismatch,
            TopologyErrorCode::BoundExceeded,
            TopologyErrorCode::GrantDenied,
            TopologyErrorCode::SelfSend,
            TopologyErrorCode::RecipientClosed,
            TopologyErrorCode::RecipientAttemptTerminal,
            TopologyErrorCode::RecipientRecoveryRequired,
            TopologyErrorCode::ReplyInvalid,
            TopologyErrorCode::MailboxFull,
            TopologyErrorCode::SpawnBudget,
            TopologyErrorCode::Capacity,
            TopologyErrorCode::RequestKeyConflict,
            TopologyErrorCode::DepthExhausted,
            TopologyErrorCode::TemplateNotSpawnable,
            TopologyErrorCode::TemplateUnknown,
            TopologyErrorCode::MintUnavailable,
            TopologyErrorCode::MintHookFailed,
            TopologyErrorCode::InputNotSealed,
            TopologyErrorCode::WorkspaceIdentityMismatch,
            TopologyErrorCode::WorkspaceNotOwned,
            TopologyErrorCode::WorkspaceReferenced,
            TopologyErrorCode::ModelNotAdmitted,
            TopologyErrorCode::SubmitRootForbidden,
            TopologyErrorCode::SubmitStaleToken,
            TopologyErrorCode::SubmitDuplicateConflict,
            TopologyErrorCode::LiveWork,
            TopologyErrorCode::TreeRestoredPaused,
            TopologyErrorCode::OperationRecoveryRequired,
            TopologyErrorCode::CandidateUnknown,
            TopologyErrorCode::NotSealed,
            TopologyErrorCode::NotAuthorized,
            TopologyErrorCode::EvidenceMismatch,
            TopologyErrorCode::TargetChanged,
            TopologyErrorCode::MergeConflict,
            TopologyErrorCode::AdoptNotApplicable,
            TopologyErrorCode::DiscardNotApplicable,
            TopologyErrorCode::FollowupNotIdle,
            TopologyErrorCode::QuotaBlocked,
            TopologyErrorCode::AttemptMismatch,
            TopologyErrorCode::NoPendingChildren,
            TopologyErrorCode::WaitCallerSettled,
            TopologyErrorCode::RecoveryLeaseHeld,
            TopologyErrorCode::RecoveryOwnerLive,
            TopologyErrorCode::RecoveryStorageUnavailable,
            TopologyErrorCode::RecoveryEffectEvidenceRefused,
            TopologyErrorCode::RecoveryVcsAmbiguous,
            TopologyErrorCode::RecoverySessionUnavailable,
        ]),
    );

    report(
        "closed vocabulary",
        "CLOSED_VOCABULARY_PINS",
        &rows,
        &failures,
    );
}
