//! The host protocol between a hosted Plakous session and whoever drives it.
//!
//!
//! Three faces of one vocabulary:
//!
//! * [`SessionIntent`] in and [`SessionObservation`] out, with the immutable
//!   [`views`] an observation carries and the [`selection`] enums both share;
//! * the two bounded in-process mailboxes in [`channel`], whose coalescing and
//!   eviction semantics every driver depends on, and the [`limits`] they and
//!   the host agree on;
//! * the `plak agent` JSONL wire: the v1 [`envelope`], the [`topology`]
//!   request/receipt shapes, and the [`jsonl`] projection of every
//!   observation, an exhaustive match so a new variant is a wire decision.
//!
//! This crate holds every type that crosses those seams and nothing that
//! decides what to do about one: dispatch, the tree runtime, the session
//! store, and the presentation projection live above it. An external
//! `plak agent` client depends on this crate, `serde` and `serde_json` —
//! never on the host. A driver with product traffic of its own (settings, an
//! overlay) threads it through the [`ProtocolExtension`] slot; the protocol
//! carries it and never reads it.
//!
//! # Framing
//!
//! One JSON object per line in both directions. Every line carries `v`, and
//! every line a client sends MUST carry `v` equal to [`envelope::SCHEMA_V`],
//! which is `1`. Every outbound object carries `v` and `kind`, and carries
//! `id` when the line answers a command; [`envelope::encode_object`] is the
//! only writer. Key order within the object is not part of the protocol: a
//! client reads fields by name. The presentation major is independent of the
//! product `SQLite` schema and of Plakous.
//!
//! # Versioning
//!
//! **v1 is frozen and growth is additive only.** Unknown additive fields on a
//! known kind are ignored, and an unknown inbound `kind` is a typed diagnostic
//! that leaves the connection up. A new field or a new kind may therefore be
//! added inside v1: a client that ignores what it does not know is unaffected.
//! A removed kind is answered like any unknown kind. Removing or renaming a
//! field, or changing the type or meaning of a field, is a new major. A missing `v` and a wrong `v` are both
//! refused with a named message.
//!
//! # Inbound kinds (client to host)
//!
//! Each carries `v`, `kind`, `id` plus the fields named here.
//!
//! | `kind` | Fields |
//! | --- | --- |
//! | `prompt` | `text`, optional `steer` (only literal `true` counts) |
//! | `interrupt`, `compact`, `clear`, `quit` | — |
//! | `fork` | optional `account` |
//! | `tree`, `resume_tree`, `stop_all` | — |
//! | `interrupt_node`, `close_node` | `agent_id` |
//! | `invoke_tool` | `name` (a [`topology::TOPOLOGY_PRESENTATION_ORDER`] entry), `arguments` (an object), optional `agent_id` |
//!
//! A `prompt` `text` is bounded by [`envelope::MAX_PROMPT_BYTES`].
//!
//! # Outbound kinds (host to client)
//!
//! `hello`, `ready`, `bye`, `error`, `status`, `busy`, `assistant`,
//! `assistant_done`, `tool`, `queue`, `turn`, `tree`, `reasoning`,
//! `approval`, `approval_resolved`, and `steer_ack`. The handshake and refusal
//! lines are built here — [`envelope::hello_line`], [`envelope::ready_line`],
//! [`envelope::bye_line`], [`envelope::diagnostic_line`] — and the rest are
//! projected by the host from its own observations. The subset in
//! [`envelope::REQUIRED_STDOUT_KINDS`] must never be dropped by a sink.
//!
//! # Correlation
//!
//! A command's `id` is echoed on the events it caused; [`envelope::JsonlCorrelation`]
//! is the rule. Turn-scoped events carry the in-flight prompt's id, else the
//! current command's. Command answers carry only the command id. A tree
//! snapshot carries an `id` only when it was asked for.
//!
//! # Topology
//!
//! [`topology`] holds the `invoke_tool` name set, the request shape of every
//! topology tool, the adopt receipt, and the tree, node, attempt, and
//! workspace row vocabularies as they are serialized into the `tree` snapshot
//! and tool receipts. [`topology::TopologyErrorCode`] is the closed refusal
//! vocabulary; its wire spelling is `snake_case`.

pub mod channel;
pub mod envelope;
pub mod intent;
pub mod jsonl;
pub mod limits;
pub mod observation;
pub mod selection;
pub mod topology;
pub mod views;

pub use channel::{
    IntentAcknowledgement, IntentDelivery, IntentReceiver, IntentSendError, IntentSender,
    ObservationReceiver, ObservationSender, ObservationSink, intent_channel, observation_channel,
};
pub use intent::{
    ApprovalChoice, Core, Draft, DraftImage, Never, ObservationExtension, ProtocolExtension,
    SessionIntent,
};
pub use observation::{
    ContextCapacityRecovery, ModelRetryNotice, PendingContinuation, QueueDrainState, QueueItem,
    SessionObservation, TurnLifecycle, TurnPhase,
};
pub use selection::{CompositionSelection, ProtocolSelection};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod wire_identity_goldens;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod round_trip_tests {
    use crate::envelope::{
        JsonlCorrelation, MAX_PROMPT_BYTES, REQUIRED_STDOUT_KINDS, SCHEMA_V, bye_line,
        diagnostic_line, encode_object, hello_line, ready_line, required_stdout_kind,
    };
    use crate::topology::{
        AdoptCandidateRequest, AdoptOutcome, AttemptId, AttemptScopedRequest, AttemptStatus,
        CandidateId, Delegation, ListAgentsRequest, MessagePriority, MessagePurpose, NodeStatus,
        SendAgentMessageRequest, SpawnAgentRequest, SpawnWorkspace, SubmitResultRequest,
        TemplateName, Tier, TopologyErrorCode, WaitAgentRequest, WorkspaceIdRequest, WorkspaceMode,
        WorkspaceStatus, WriterMode,
    };
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use serde_json::{Map, Value, json};
    use std::fmt::Debug;

    fn round_trip<T: Serialize + DeserializeOwned + Debug + PartialEq>(value: &T, wire: Value) {
        let encoded = serde_json::to_value(value).unwrap();
        assert_eq!(encoded, wire, "{value:?}");
        let decoded: T = serde_json::from_value(wire).unwrap();
        assert_eq!(&decoded, value);
    }

    #[test]
    fn every_topology_request_kind_round_trips() {
        round_trip(
            &SpawnAgentRequest {
                template: TemplateName::parse("review").unwrap(),
                description: "reviewer".into(),
                prompt: "review the diff".into(),
                output_schema_json: "{}".into(),
                tier: Tier::ReadOnly,
                workspace: SpawnWorkspace::Inherit,
                inputs: Some(vec!["cand-1".into()]),
                request_key: Some("k1".into()),
            },
            json!({
                "template": "review",
                "description": "reviewer",
                "prompt": "review the diff",
                "output_schema_json": "{}",
                "tier": "read_only",
                "inputs": ["cand-1"],
                "request_key": "k1",
            }),
        );
        round_trip(
            &SendAgentMessageRequest {
                agent_id: "a1".into(),
                message: "status?".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: Some(MessagePriority::Urgent),
                in_reply_to: None,
                output_schema_json: Some("{}".into()),
            },
            json!({
                "agent_id": "a1",
                "message": "status?",
                "purpose": "followup",
                "priority": "urgent",
                "in_reply_to": Value::Null,
                "output_schema_json": "{}",
            }),
        );
        round_trip(
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(true),
            },
            json!({"include_self": false, "include_completed": true}),
        );
        round_trip(
            &WaitAgentRequest {
                agent_ids: Some(vec!["a1".into(), "a2".into()]),
                timeout_ms: Some(1_000),
            },
            json!({"agent_ids": ["a1", "a2"], "timeout_ms": 1000}),
        );
        round_trip(
            &AttemptScopedRequest {
                agent_id: "a1".into(),
                expected_attempt_id: Some(AttemptId::new("a1", 1)),
            },
            json!({"agent_id": "a1", "expected_attempt_id": "a1-a1"}),
        );
        round_trip(
            &SubmitResultRequest {
                turn_token: 7,
                output_json: r#"{"ok":true}"#.into(),
            },
            json!({"turn_token": 7, "output_json": "{\"ok\":true}"}),
        );
        round_trip(
            &WorkspaceIdRequest {
                workspace_id: "w1".into(),
            },
            json!({"workspace_id": "w1"}),
        );
        round_trip(
            &AdoptCandidateRequest {
                candidate_id: CandidateId::from_persisted("c1".to_owned()),
                expected_target: Some("deadbeef".into()),
                evidence: Some(vec!["a2-a1".into()]),
            },
            json!({
                "candidate_id": "c1",
                "expected_target": "deadbeef",
                "evidence": ["a2-a1"],
            }),
        );
        round_trip(
            &AdoptOutcome {
                candidate_id: CandidateId::from_persisted("c1".to_owned()),
                target_before: "deadbeef".into(),
                target_after: "cafebabe".into(),
                files_changed: 3,
            },
            json!({
                "candidate_id": "c1",
                "target_before": "deadbeef",
                "target_after": "cafebabe",
                "files_changed": 3,
            }),
        );
    }

    #[test]
    fn every_row_vocabulary_round_trips() {
        for (value, wire) in [
            (Tier::ReadOnly, "read_only"),
            (Tier::Execute, "execute"),
            (Tier::Mutate, "mutate"),
        ] {
            round_trip(&value, Value::from(wire));
            assert_eq!(value.as_str(), wire);
            assert_eq!(Tier::parse(wire), Ok(value));
        }
        for (value, wire) in [
            (Delegation::Explicit, "explicit"),
            (Delegation::Proactive, "proactive"),
        ] {
            round_trip(&value, Value::from(wire));
            assert_eq!(Delegation::parse(wire), Ok(value));
        }
        for (value, wire) in [
            (NodeStatus::Pending, "pending"),
            (NodeStatus::Running, "running"),
            (NodeStatus::Paused, "paused"),
            (NodeStatus::Sealing, "sealing"),
            (NodeStatus::Completed, "completed"),
            (NodeStatus::Interrupted, "interrupted"),
            (NodeStatus::Failed, "failed"),
            (NodeStatus::Closed, "closed"),
            (NodeStatus::RecoveryRequired, "recovery_required"),
        ] {
            round_trip(&value, Value::from(wire));
        }
        for (value, wire) in [
            (AttemptStatus::Running, "running"),
            (AttemptStatus::Submitted, "submitted"),
            (AttemptStatus::Completed, "completed"),
            (
                AttemptStatus::CompletedWithoutSubmission,
                "completed_without_submission",
            ),
            (AttemptStatus::Failed, "failed"),
            (AttemptStatus::Cancelled, "cancelled"),
            (AttemptStatus::RecoveryRequired, "recovery_required"),
            (AttemptStatus::Closed, "closed"),
            (AttemptStatus::Blocked, "blocked"),
            (AttemptStatus::Adopted, "adopted"),
        ] {
            round_trip(&value, Value::from(wire));
        }
        for (value, wire) in [
            (WorkspaceStatus::Active, "active"),
            (WorkspaceStatus::Adopting, "adopting"),
            (WorkspaceStatus::Discarding, "discarding"),
            (WorkspaceStatus::CleanRemoved, "clean_removed"),
            (WorkspaceStatus::Adopted, "adopted"),
            (WorkspaceStatus::Discarded, "discarded"),
        ] {
            round_trip(&value, Value::from(wire));
        }
        round_trip(&WriterMode::SingleLease, Value::from("single_lease"));
        assert_eq!(
            WriterMode::parse("single_lease"),
            Ok(WriterMode::SingleLease)
        );
        for (value, wire) in [
            (WorkspaceMode::Inherit, "inherit"),
            (WorkspaceMode::Snapshot, "snapshot"),
            (WorkspaceMode::Fork, "fork"),
        ] {
            round_trip(&value, Value::from(wire));
            assert_eq!(value.as_str(), wire);
        }
        for (value, wire) in [
            (MessagePurpose::Coordinate, "coordinate"),
            (MessagePurpose::Finding, "finding"),
            (MessagePurpose::Question, "question"),
            (MessagePurpose::Reply, "reply"),
            (MessagePurpose::Followup, "followup"),
            (MessagePurpose::Result, "result"),
        ] {
            round_trip(&value, Value::from(wire));
        }
        for (value, wire) in [
            (MessagePriority::Deferred, "deferred"),
            (MessagePriority::Urgent, "urgent"),
        ] {
            round_trip(&value, Value::from(wire));
        }
        round_trip(&AttemptId::new("agent-1", 3), Value::from("agent-1-a3"));
        round_trip(
            &TemplateName::parse("review").unwrap(),
            Value::from("review"),
        );
    }

    #[test]
    fn every_error_code_has_a_distinct_snake_case_wire_form() {
        let codes = [
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
        ];
        let mut seen = std::collections::BTreeSet::new();
        for code in codes {
            let wire = serde_json::to_value(code).unwrap();
            let text = wire.as_str().unwrap().to_owned();
            assert!(
                text.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{text}"
            );
            assert!(seen.insert(text), "{code:?} duplicates a wire form");
            let decoded: TopologyErrorCode = serde_json::from_value(wire).unwrap();
            assert_eq!(decoded, code);
        }
        assert_eq!(seen.len(), codes.len());
    }

    #[test]
    fn every_envelope_line_carries_the_frame_and_its_kind() {
        let lines = [
            hello_line(
                Some("c1"),
                "session-1",
                "tree-1",
                Some("codex"),
                Some("gpt-5.6-sol"),
                Some("medium"),
                "direct",
                "live",
                None,
                None,
                None,
                None,
                Some(12),
            ),
            ready_line(Some("c1"), Some(13)),
            bye_line(Some("c1")),
            diagnostic_line(Some("c1"), "unknown JSONL kind `login`"),
        ];
        for line in &lines {
            assert!(!line.contains('\n'), "{line}");
            let value: Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["v"], Value::from(SCHEMA_V));
            assert_eq!(value["id"], Value::from("c1"));
            assert!(value["kind"].is_string(), "{line}");
        }
        let kinds: Vec<String> = lines
            .iter()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(kinds, ["hello", "ready", "bye", "error"]);
        for kind in ["hello", "ready", "bye", "error"] {
            assert!(REQUIRED_STDOUT_KINDS.contains(&kind));
        }
    }

    #[test]
    fn required_stdout_kinds_are_exactly_the_undroppable_set() {
        for kind in REQUIRED_STDOUT_KINDS {
            let mut fields = Map::new();
            fields.insert("kind".to_owned(), Value::from(kind));
            assert!(required_stdout_kind(&encode_object(fields)), "{kind}");
        }
        for kind in ["status", "busy", "assistant", "queue", "reasoning"] {
            let mut fields = Map::new();
            fields.insert("kind".to_owned(), Value::from(kind));
            assert!(!required_stdout_kind(&encode_object(fields)), "{kind}");
        }
        assert!(!required_stdout_kind("not json"));
    }

    #[test]
    fn correlation_scopes_the_echoed_id() {
        let correlation = JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: Some("c1"),
            pending_tree_snapshot_id: Some("t1"),
        };
        assert_eq!(correlation.turn_scoped_id(), Some("p1"));
        assert_eq!(correlation.command_scoped_id(), Some("c1"));
        assert_eq!(correlation.tree_snapshot_id(true), Some("t1"));
        assert_eq!(correlation.tree_snapshot_id(false), None);
        let empty = JsonlCorrelation::default();
        assert_eq!(empty.turn_scoped_id(), None);
        let mut fields = Map::new();
        JsonlCorrelation::insert_id(&mut fields, Some(""));
        assert!(fields.is_empty());
        JsonlCorrelation::insert_id(&mut fields, Some("c1"));
        assert_eq!(fields["id"], Value::from("c1"));
    }

    #[test]
    fn prompt_bound_is_the_run_admission_bound() {
        assert_eq!(MAX_PROMPT_BYTES, 64 * 1024);
        assert_eq!(SCHEMA_V, 1);
    }
}

/// Host facts that determine a node's prompt completion channel, not its tool grants.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PromptHostFacts {
    /// True when this node's host starts a new turn on the node's tracked work:
    /// an attached root with continuation enabled, and every child of the
    /// product's agent tree. False for JSONL roots and embedders without a
    /// wake loop.
    pub host_wakes: bool,
}
