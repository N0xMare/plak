//! The v1 JSONL projection of the session observations.
//!
//! Lossy on purpose: presentation chrome and product extension traffic are
//! dropped. `tool`, `error`, `turn`, and `tree` are always produced when the
//! matching observation is observed — including a tool's terminal line, which
//! carries the host's `duration_ms` stamp. The match is
//! exhaustive with no wildcard so that adding an observation variant is a
//! compile-time decision about its wire form.

use serde_json::{Map, Value};

use crate::envelope::{JsonlCorrelation, encode_object};
use crate::intent::{ObservationExtension, ProtocolExtension};
use crate::observation::{QueueDrainState, SessionObservation};
use crate::topology::CandidateId;
use crate::views::ActivityKind;
use crate::views::transcript::{NoticePlacement, NoticeTone, TranscriptRow};

/// Stable JSONL label for one host-populated activity kind.
const fn activity_kind_label(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Tool => "tool",
        ActivityKind::Process => "process",
        ActivityKind::Model => "model",
        ActivityKind::Agent => "agent",
        ActivityKind::Compose => "compose",
        ActivityKind::Compact => "compact",
    }
}

/// Content-free observation kind label for outbound JSONL diagnostics spans.
#[must_use]
pub fn host_event_kind<P: ProtocolExtension>(event: &SessionObservation<P>) -> &'static str {
    match event {
        SessionObservation::JobStarted { .. } => "job_started",
        SessionObservation::JobPromoted { .. } => "job_promoted",
        SessionObservation::JobTerminal { .. } => "job_terminal",
        SessionObservation::ChildLive { .. } => "child_live",
        SessionObservation::PresentationSnapshot(_) => "presentation",
        SessionObservation::ProviderSnapshot { .. } => "provider_snapshot",
        SessionObservation::Activity { .. } => "activity",
        SessionObservation::AgentAttached(_) => "attached",
        SessionObservation::Row(_) => "row",
        SessionObservation::Status { .. } => "status",
        SessionObservation::QueueSnapshot { .. } => "queue",
        SessionObservation::ContextCapacityRecovery(_) => "context_capacity_recovery",
        SessionObservation::ResumeSessions(_) => "resume_sessions",
        SessionObservation::ToolInvocationOrigin { .. } => "tool_invocation_origin",
        SessionObservation::PromptAdmitted(_) => "prompt_admitted",
        SessionObservation::PromptStarted(_) => "prompt_started",
        SessionObservation::PromptCorrelationReset => "prompt_correlation_reset",
        SessionObservation::IntentStarted(_) => "intent_started",
        SessionObservation::DraftAccepted(_) => "draft_accepted",
        SessionObservation::DraftRejected(_) => "draft_rejected",
        SessionObservation::DraftRestored(_) => "draft_restored",
        SessionObservation::QueueEditApplied { .. } => "queue_edit_applied",
        SessionObservation::QueueEditRefused { .. } => "queue_edit_refused",
        SessionObservation::SetBusy(_) => "busy",
        SessionObservation::PromptHistorySnapshot(_) => "prompt_history",
        SessionObservation::UserMessage { .. } => "user_message",
        SessionObservation::AssistantDelta(_) => "assistant",
        SessionObservation::AssistantDone => "assistant_done",
        SessionObservation::ModelRefusal { .. } => "model_refusal",
        SessionObservation::ToolLine { .. } => "tool",
        SessionObservation::ModelRetry(_) => "retry",
        SessionObservation::Binding { .. } => "binding",
        SessionObservation::ClearView => "clear_view",
        SessionObservation::Usage(_) => "usage",
        SessionObservation::TurnUsage { .. } => "turn",
        SessionObservation::TurnLifecycle(_) => "turn_lifecycle",
        SessionObservation::ReasoningDelta { .. } => "reasoning",
        SessionObservation::ReasoningSealed { .. } => "reasoning_sealed",
        SessionObservation::ApprovalRequired { .. } => "approval_required",
        SessionObservation::ApprovalResolved { .. } => "approval_resolved",
        SessionObservation::SteerAck { .. } => "steer_ack",
        SessionObservation::SubscriptionUsage(_) => "subscription_usage",
        SessionObservation::UsageLimits(_) => "usage_limits",
        SessionObservation::SessionInfo(_) => "session_info",
        SessionObservation::TemplateReadiness(_) => "template_readiness",
        SessionObservation::ReattachReady => "reattach_ready",
        SessionObservation::Recovered { .. } => "recovered",
        SessionObservation::SessionOpening => "session_opening",
        SessionObservation::ModelRequestStarted => "model_request_started",
        SessionObservation::SessionOpened { .. } => "session_opened",
        SessionObservation::TopologyNavigatorSnapshot(_) => "tree",
        SessionObservation::CallDetail(_) => "call_detail",
        SessionObservation::AgentTranscript(_) => "agent_transcript",
        SessionObservation::TopologyNavigatorUnavailable { .. } => "tree_unavailable",
        SessionObservation::PendingContinuation { .. } => "pending_continuation",
        SessionObservation::LastUserPromptAt(_) => "last_user_prompt_at",
        SessionObservation::Extension(extension) => extension.kind_label(),
    }
}

/// Project one observation to its v1 line, or `None` for an observation the
/// wire does not carry.
#[must_use]
#[allow(
    clippy::too_many_lines,
    clippy::match_same_arms,
    reason = "one exhaustive match is the wire contract: every variant is decided here"
)]
pub fn project_host_event<P: ProtocolExtension>(
    event: &SessionObservation<P>,
    correlation: JsonlCorrelation<'_>,
) -> Option<String> {
    let mut fields = Map::new();
    match event {
        SessionObservation::JobStarted {
            job_id,
            owner,
            command,
        } => {
            fields.insert("kind".into(), Value::from("job_started"));
            fields.insert("job_id".into(), Value::from(job_id.as_str()));
            fields.insert("owner".into(), Value::from(owner.as_str()));
            fields.insert("command".into(), Value::from(command.as_str()));
        }
        SessionObservation::JobPromoted { job_id, owner } => {
            fields.insert("kind".into(), Value::from("job_promoted"));
            fields.insert("job_id".into(), Value::from(job_id.as_str()));
            fields.insert("owner".into(), Value::from(owner.as_str()));
        }
        SessionObservation::JobTerminal {
            job_id,
            owner,
            status,
            exit_code,
            stop_reason,
            elapsed_ms,
            output,
        } => {
            fields.insert("kind".into(), Value::from("job_terminal"));
            fields.insert("job_id".into(), Value::from(job_id.as_str()));
            fields.insert("owner".into(), Value::from(owner.as_str()));
            fields.insert("status".into(), Value::from(status.as_str()));
            fields.insert("elapsed_ms".into(), Value::from(*elapsed_ms));
            fields.insert("output".into(), Value::from(output.as_str()));
            if let Some(code) = exit_code {
                fields.insert("exit_code".into(), Value::from(*code));
            }
            if let Some(reason) = stop_reason {
                fields.insert("stop_reason".into(), Value::from(reason.as_str()));
            }
        }
        SessionObservation::Status {
            phase,
            queue_len,
            detail,
        } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("status"));
            fields.insert("phase".to_owned(), Value::from(phase.as_str()));
            fields.insert("queue_len".to_owned(), Value::from(*queue_len as u64));
            if let Some(detail) = detail {
                fields.insert("detail".to_owned(), Value::from(detail.as_str()));
            }
        }
        SessionObservation::SetBusy(busy) => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("busy"));
            fields.insert("busy".to_owned(), Value::from(*busy));
        }
        SessionObservation::AssistantDelta(delta) => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("assistant"));
            fields.insert("text".to_owned(), Value::from(delta.as_str()));
        }
        SessionObservation::AssistantDone => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("assistant_done"));
        }
        SessionObservation::ModelRefusal { detail } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("model_refusal"));
            if let Some(detail) = detail {
                fields.insert("detail".to_owned(), Value::from(detail.as_str()));
            }
            fields.insert(
                "message".to_owned(),
                Value::from("model refused the request"),
            );
        }
        SessionObservation::ToolLine {
            call_id,
            name,
            state,
            subject,
            detail,
            parent_call_id,
            cell_id,
            kind: activity_kind,
            duration_ms,
            started_at_ms: _,
            finished_at_ms: _,
            wait: _,
        } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("tool"));
            fields.insert("name".to_owned(), Value::from(name.as_str()));
            fields.insert("state".to_owned(), Value::from(state.as_str()));
            // Additive v1 fields; SCHEMA_V stays 1.
            fields.insert(
                "activity_kind".to_owned(),
                Value::from(activity_kind_label(*activity_kind)),
            );
            if let Some(parent_call_id) = parent_call_id {
                fields.insert(
                    "parent_call_id".to_owned(),
                    Value::from(parent_call_id.as_str()),
                );
            }
            if let Some(cell_id) = cell_id {
                fields.insert("cell_id".to_owned(), Value::from(cell_id.as_str()));
            }
            if let Some(call_id) = call_id {
                fields.insert("call_id".to_owned(), Value::from(call_id.as_str()));
            }
            if let Some(subject) = subject {
                fields.insert("subject".to_owned(), Value::from(subject.as_str()));
            }
            if let Some(detail) = detail {
                fields.insert("detail".to_owned(), Value::from(detail.as_str()));
            }
            // Additive v1 field. The host stamps a duration on
            // every line it can, and the terminal line of a tool that ran is
            // always one of them; dropping those lines left a client with a
            // `running` row that never terminalised, against this module's own
            // contract sentence and the protocol's required stdout kinds.
            if let Some(duration_ms) = duration_ms {
                fields.insert("duration_ms".to_owned(), Value::from(*duration_ms));
            }
        }
        SessionObservation::Recovered {
            claim,
            cause,
            operations,
        } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.command_scoped_id());
            fields.insert("kind".to_owned(), Value::from("recovered"));
            if let Some(claim) = claim {
                fields.insert("claim".to_owned(), Value::from(claim.as_str()));
            }
            if let Some(cause) = cause {
                fields.insert("cause".to_owned(), Value::from(cause.as_str()));
            }
            fields.insert("operations".to_owned(), Value::from(operations.clone()));
        }
        SessionObservation::ModelRetry(retry) => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("retry"));
            fields.insert("attempt".to_owned(), Value::from(retry.next_attempt));
            fields.insert("max_attempts".to_owned(), Value::from(retry.max_attempts));
            fields.insert("delay_ms".to_owned(), Value::from(retry.delay_ms));
            fields.insert(
                "cause_kind".to_owned(),
                Value::from(retry.cause_kind.as_str()),
            );
            fields.insert("cause".to_owned(), Value::from(retry.cause.as_str()));
            fields.insert(
                "output_discarded".to_owned(),
                Value::from(retry.output_discarded),
            );
        }
        SessionObservation::Row(TranscriptRow::Notice(notice))
            if notice.placement() != NoticePlacement::SessionLog =>
        {
            JsonlCorrelation::insert_id(&mut fields, correlation.command_scoped_id());
            if notice.tone == NoticeTone::Error {
                fields.insert("kind".to_owned(), Value::from("error"));
                fields.insert("message".to_owned(), Value::from(notice.text.as_str()));
            } else {
                fields.insert("kind".to_owned(), Value::from("status"));
                fields.insert("phase".to_owned(), Value::from("note"));
                fields.insert("queue_len".to_owned(), Value::from(0_u64));
                fields.insert("detail".to_owned(), Value::from(notice.text.as_str()));
            }
        }
        SessionObservation::QueueSnapshot { items, drain } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("queue"));
            fields.insert(
                "drain".to_owned(),
                Value::from(match drain {
                    QueueDrainState::LiveAutoDrain => "live_auto_drain",
                    QueueDrainState::RestoredPaused => "restored_paused",
                }),
            );
            let queue = items
                .iter()
                .map(|item| {
                    Value::Object(Map::from_iter([
                        ("id".to_owned(), Value::from(item.id.as_str())),
                        ("revision".to_owned(), Value::from(item.revision)),
                        ("text".to_owned(), Value::from(item.text.as_str())),
                        ("rejected".to_owned(), Value::from(item.rejected)),
                    ]))
                })
                .collect();
            fields.insert("items".to_owned(), Value::Array(queue));
        }
        SessionObservation::TopologyNavigatorSnapshot(snapshot) => {
            JsonlCorrelation::insert_id(
                &mut fields,
                correlation.tree_snapshot_id(snapshot.solicited),
            );
            fields.insert("kind".to_owned(), Value::from("tree"));
            fields.insert("tree_id".to_owned(), Value::from(snapshot.tree_id.as_str()));
            fields.insert("revision".to_owned(), Value::from(snapshot.revision));
            fields.insert("state".to_owned(), Value::from(snapshot.state.as_str()));
            fields.insert(
                "restored_paused".to_owned(),
                Value::from(snapshot.restored_paused()),
            );
            let nodes = snapshot
                .nodes
                .iter()
                .map(|node| {
                    let mut row = Map::new();
                    row.insert("agent_id".to_owned(), Value::from(node.agent_id.as_str()));
                    if let Some(parent) = node.parent_agent_id.as_deref() {
                        row.insert("parent_agent_id".to_owned(), Value::from(parent));
                    }
                    if !node.model_account.is_empty() {
                        row.insert(
                            "account".to_owned(),
                            Value::from(node.model_account.as_str()),
                        );
                    }
                    row.insert("depth".to_owned(), Value::from(u64::from(node.depth)));
                    row.insert("status".to_owned(), Value::from(node.status.as_str()));
                    row.insert(
                        "description".to_owned(),
                        Value::from(node.description.as_str()),
                    );
                    row.insert("prompt".to_owned(), Value::from(node.prompt.as_str()));
                    row.insert(
                        "workspace_path".to_owned(),
                        Value::from(node.workspace_path.as_str()),
                    );
                    row.insert("active_turn".to_owned(), Value::from(node.active_turn));
                    if let Some(waiting_on) = &node.waiting_on {
                        row.insert("waiting_on".to_owned(), serde_json::json!(waiting_on));
                    }
                    if let Some(progress) = &node.live_progress {
                        row.insert(
                            "live_progress".to_owned(),
                            serde_json::json!({
                                "turn_id": progress.turn_id,
                                "activity": progress.activity,
                                "tool_calls": progress.tool_calls,
                                "last_activity_unix_ms": progress.last_activity_unix_ms,
                                "input_tokens": progress.input_tokens,
                                "output_tokens": progress.output_tokens,
                                "reasoning_tokens": progress.reasoning_tokens,
                                "cached_input_tokens": progress.cached_input_tokens,
                                "cache_write_tokens": progress.cache_write_tokens,
                            }),
                        );
                    }
                    // Lifecycle observations. Absent keys mean the host does
                    // not know, never a default: a JSONL client that reads
                    // `tier: "read_only"` where the host held nothing would be
                    // told the node cannot write.
                    row.insert(
                        "workspace_mode".to_owned(),
                        Value::from(node.workspace_mode.as_str()),
                    );
                    if let Some(tier) = node.tier {
                        row.insert("tier".to_owned(), Value::from(tier.as_str()));
                    }
                    if let Some(template) = node.template.as_deref() {
                        row.insert("template".to_owned(), Value::from(template));
                    }
                    if let Some(attempt) = node.attempt_status {
                        row.insert("attempt".to_owned(), Value::from(attempt.as_str()));
                    }
                    if let Some(seq) = node.attempt_seq {
                        row.insert("attempt_seq".to_owned(), Value::from(u64::from(seq)));
                    }
                    if let Some(workspace_status) = node.workspace_status {
                        row.insert(
                            "workspace_status".to_owned(),
                            Value::from(workspace_status.as_str()),
                        );
                    }
                    if let Some(candidate) = node.candidate_id.as_ref().map(CandidateId::as_str) {
                        row.insert("candidate".to_owned(), Value::from(candidate));
                    }
                    if let Some(blocked) = node.blocked.as_ref() {
                        let mut refusal = Map::new();
                        refusal.insert("quota".to_owned(), Value::from(blocked.quota.as_str()));
                        refusal.insert("account".to_owned(), Value::from(blocked.account.as_str()));
                        if let Some(resets_at) = blocked.resets_at_unix {
                            refusal.insert("resets_at".to_owned(), Value::from(resets_at));
                        }
                        row.insert("blocked".to_owned(), Value::Object(refusal));
                    }
                    Value::Object(row)
                })
                .collect();
            fields.insert("nodes".to_owned(), Value::Array(nodes));
        }
        // `turn` means the drive finished. A live cumulative usage
        // snapshot (`terminal: false`) is TUI meter chrome; projecting it as
        // `turn` would tell a client the turn ended while the model is still
        // running, so it is dropped here.
        SessionObservation::TurnUsage {
            terminal: false, ..
        } => return None,
        SessionObservation::TurnUsage {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cached_input_tokens,
            cache_write_tokens,
            model_operations,
            peak_prepared_input_tokens,
            terminal: true,
            ..
        } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("turn"));
            fields.insert("outcome".to_owned(), Value::from("usage"));
            fields.insert("input_tokens".to_owned(), Value::from(*input_tokens));
            fields.insert("output_tokens".to_owned(), Value::from(*output_tokens));
            fields.insert(
                "reasoning_tokens".to_owned(),
                Value::from(*reasoning_tokens),
            );
            fields.insert(
                "cached_input_tokens".to_owned(),
                Value::from(*cached_input_tokens),
            );
            fields.insert(
                "cache_write_tokens".to_owned(),
                Value::from(*cache_write_tokens),
            );
            // Growth facts, additive. `input_tokens` sums over operations, so
            // a client measuring how far a turn grew against the window reads
            // the peak instead; the operation count says how many requests
            // that sum covers. Absent rather than zero when unknown.
            if let Some(operations) = model_operations {
                fields.insert("model_operations".to_owned(), Value::from(*operations));
            }
            if let Some(peak) = peak_prepared_input_tokens {
                fields.insert("peak_prepared_input_tokens".to_owned(), Value::from(*peak));
            }
        }
        // The correlated lifecycle observation. `phase` is projected from the
        // canonical `TurnResult` only — `unresolved` reports missing terminal
        // evidence and never invents an outcome.
        SessionObservation::TurnLifecycle(lifecycle) => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("turn_lifecycle"));
            fields.insert(
                "turn_id".to_owned(),
                Value::from(lifecycle.turn_id.as_str()),
            );
            fields.insert("phase".to_owned(), Value::from(lifecycle.phase.as_str()));
            if let Some(kind) = &lifecycle.model_error_kind {
                fields.insert("model_error_kind".to_owned(), Value::from(kind.as_str()));
            }
            if let Some(status) = lifecycle.http_status {
                fields.insert("http_status".to_owned(), Value::from(status));
            }
        }
        SessionObservation::TopologyNavigatorUnavailable { reason } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.command_scoped_id());
            fields.insert("kind".to_owned(), Value::from("error"));
            fields.insert("message".to_owned(), Value::from(reason.as_str()));
        }
        // Additive v1 kinds. A v1 client that ignores unknown
        // kinds is unaffected, and none of them carries a decision address.
        SessionObservation::ReasoningDelta { location_key, text } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("reasoning"));
            fields.insert("location".to_owned(), Value::from(location_key.as_str()));
            fields.insert("text".to_owned(), Value::from(text.as_str()));
        }
        SessionObservation::ReasoningSealed { location_key } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("reasoning"));
            fields.insert("location".to_owned(), Value::from(location_key.as_str()));
            fields.insert("sealed".to_owned(), Value::from(true));
        }
        SessionObservation::ApprovalRequired {
            approval_id,
            call_id,
            tool_name,
            subject,
            prompt,
            expires_at_ms,
        } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("approval"));
            fields.insert("approval_id".to_owned(), Value::from(approval_id.as_str()));
            fields.insert("name".to_owned(), Value::from(tool_name.as_str()));
            fields.insert("expires_at_ms".to_owned(), Value::from(*expires_at_ms));
            if let Some(call_id) = call_id {
                fields.insert("call_id".to_owned(), Value::from(call_id.as_str()));
            }
            if let Some(subject) = subject {
                fields.insert("subject".to_owned(), Value::from(subject.as_str()));
            }
            if let Some(prompt) = prompt {
                fields.insert("prompt".to_owned(), Value::from(prompt.as_str()));
            }
        }
        SessionObservation::ApprovalResolved {
            approval_id,
            resolution,
        } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("approval_resolved"));
            fields.insert("approval_id".to_owned(), Value::from(approval_id.as_str()));
            fields.insert("resolution".to_owned(), Value::from(resolution.as_str()));
        }
        SessionObservation::SteerAck { state, position } => {
            JsonlCorrelation::insert_id(&mut fields, correlation.turn_scoped_id());
            fields.insert("kind".to_owned(), Value::from("steer_ack"));
            fields.insert("state".to_owned(), Value::from(state.as_str()));
            if let Some(position) = position {
                fields.insert(
                    "position".to_owned(),
                    Value::from(u64::try_from(*position).unwrap_or(u64::MAX)),
                );
            }
        }
        // Presentation chrome, admission receipts, and product extension
        // traffic have no v1 line. Listed one by one so a new observation
        // variant is a decision here, not a silent drop.
        SessionObservation::PresentationSnapshot(_)
        | SessionObservation::ProviderSnapshot { .. }
        | SessionObservation::Activity { .. }
        | SessionObservation::AgentAttached(_)
        | SessionObservation::Row(_)
        | SessionObservation::ContextCapacityRecovery(_)
        | SessionObservation::ResumeSessions(_)
        | SessionObservation::ToolInvocationOrigin { .. }
        | SessionObservation::PromptAdmitted(_)
        | SessionObservation::PromptStarted(_)
        | SessionObservation::PromptCorrelationReset
        | SessionObservation::IntentStarted(_)
        | SessionObservation::DraftAccepted(_)
        | SessionObservation::DraftRejected(_)
        | SessionObservation::DraftRestored(_)
        | SessionObservation::QueueEditApplied { .. }
        | SessionObservation::QueueEditRefused { .. }
        | SessionObservation::PromptHistorySnapshot(_)
        | SessionObservation::UserMessage { .. }
        | SessionObservation::Binding { .. }
        | SessionObservation::ClearView
        | SessionObservation::Usage(_)
        | SessionObservation::SubscriptionUsage(_)
        | SessionObservation::UsageLimits(_)
        | SessionObservation::SessionInfo(_)
        | SessionObservation::TemplateReadiness(_)
        | SessionObservation::ReattachReady
        | SessionObservation::SessionOpening
        | SessionObservation::ModelRequestStarted
        | SessionObservation::SessionOpened { .. }
        | SessionObservation::CallDetail(_)
        | SessionObservation::AgentTranscript(_)
        | SessionObservation::ChildLive { .. }
        | SessionObservation::PendingContinuation { .. }
        | SessionObservation::LastUserPromptAt(_)
        | SessionObservation::Extension(_) => return None,
    }
    Some(encode_object(fields))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::{host_event_kind, project_host_event};
    use crate::envelope::JsonlCorrelation;
    use crate::intent::{Core, ObservationExtension, ProtocolExtension};
    use crate::observation::{SessionObservation, TurnLifecycle, TurnPhase};
    use crate::views::transcript::{NoticeScope, TranscriptRow};

    #[test]
    fn child_live_is_addressed_in_process_and_omitted_from_jsonl_v1() {
        let event: SessionObservation = SessionObservation::ChildLive {
            agent_id: "c1".into(),
            turn_id: "turn-c1".into(),
            op: Box::new(SessionObservation::AssistantDelta("child text".into())),
        };
        assert_eq!(host_event_kind(&event), "child_live");
        assert!(
            project_host_event(
                &event,
                JsonlCorrelation {
                    prompt_id: Some("root-command"),
                    omit_unknown_turn_id: false,
                    command_id: None,
                    pending_tree_snapshot_id: None,
                }
            )
            .is_none()
        );
    }

    #[test]
    fn last_user_prompt_time_is_in_process_and_omitted_from_jsonl_v1() {
        for event in [
            SessionObservation::<Core>::LastUserPromptAt(Some(1_790_000_000_000)),
            SessionObservation::<Core>::LastUserPromptAt(None),
        ] {
            assert_eq!(host_event_kind(&event), "last_user_prompt_at");
            assert_eq!(
                project_host_event(&event, JsonlCorrelation::default()),
                None
            );
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Overlay;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct OpenOverlay;

    impl ObservationExtension for OpenOverlay {
        fn latest_state(&self) -> bool {
            true
        }

        fn same_kind(&self, _other: &Self) -> bool {
            true
        }

        fn kind_label(&self) -> &'static str {
            "overlay"
        }
    }

    impl ProtocolExtension for Overlay {
        type Intent = OpenOverlay;
        type Observation = OpenOverlay;
    }

    #[test]
    fn extension_traffic_never_reaches_the_wire() {
        let observation: SessionObservation<Overlay> = SessionObservation::Extension(OpenOverlay);
        assert_eq!(
            project_host_event(&observation, JsonlCorrelation::default()),
            None
        );
        assert_eq!(host_event_kind(&observation), "overlay");
    }

    #[test]
    fn model_refusal_projects_supplied_detail_without_inventing_one() {
        for detail in [
            None,
            Some("category=fixture; explanation=declined".to_owned()),
        ] {
            let event: SessionObservation<Core> = SessionObservation::ModelRefusal {
                detail: detail.clone(),
            };
            let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value["kind"], "model_refusal");
            assert_eq!(value["message"], "model refused the request");
            assert_eq!(
                value.get("detail").and_then(serde_json::Value::as_str),
                detail.as_deref()
            );
            assert_eq!(host_event_kind(&event), "model_refusal");
            assert!(crate::envelope::REQUIRED_STDOUT_KINDS.contains(&"model_refusal"));
        }
    }

    #[test]
    fn a_model_retry_projects_one_required_retry_frame() {
        let event: SessionObservation<Core> =
            SessionObservation::ModelRetry(crate::observation::ModelRetryNotice {
                next_attempt: 3,
                max_attempts: 4,
                delay_ms: 2_000,
                cause_kind: "http_status".to_owned(),
                cause: "provider busy (HTTP 503)".to_owned(),
                output_discarded: false,
            });
        let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["kind"], "retry");
        assert_eq!(value["attempt"], 3);
        assert_eq!(value["max_attempts"], 4);
        assert_eq!(value["delay_ms"], 2_000);
        assert_eq!(value["cause_kind"], "http_status");
        assert_eq!(value["cause"], "provider busy (HTTP 503)");
        assert_eq!(value["output_discarded"], false);
        assert_eq!(host_event_kind(&event), "retry");
        assert!(crate::envelope::required_stdout_kind(&line));
    }

    #[test]
    fn turn_lifecycle_projects_phase_and_error_context() {
        let event: SessionObservation<Core> = SessionObservation::TurnLifecycle(TurnLifecycle {
            turn_id: "00000000-0000-4000-8000-0000000000aa".to_owned(),
            phase: TurnPhase::Failed,
            model_error_kind: Some("RateLimited".to_owned()),
            http_status: Some(429),
        });
        let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["kind"], "turn_lifecycle");
        assert_eq!(value["turn_id"], "00000000-0000-4000-8000-0000000000aa");
        assert_eq!(value["phase"], "failed");
        assert_eq!(value["model_error_kind"], "RateLimited");
        assert_eq!(value["http_status"], 429);
        assert_eq!(host_event_kind(&event), "turn_lifecycle");
        assert!(
            crate::envelope::REQUIRED_STDOUT_KINDS.contains(&"turn_lifecycle"),
            "lifecycle evidence is a required stdout kind"
        );
    }

    #[test]
    fn turn_lifecycle_omits_absent_error_context() {
        let event: SessionObservation<Core> = SessionObservation::TurnLifecycle(TurnLifecycle {
            turn_id: "00000000-0000-4000-8000-0000000000bb".to_owned(),
            phase: TurnPhase::Unresolved,
            model_error_kind: None,
            http_status: None,
        });
        let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["phase"], "unresolved");
        assert!(value.get("model_error_kind").is_none());
        assert!(value.get("http_status").is_none());
    }

    #[test]
    fn turn_phase_labels_match_the_serde_form() {
        for (phase, label) in [
            (TurnPhase::Admitted, "admitted"),
            (TurnPhase::Completed, "completed"),
            (TurnPhase::Refused, "refused"),
            (TurnPhase::Cancelled, "cancelled"),
            (TurnPhase::Failed, "failed"),
            (TurnPhase::RecoveryRequired, "recovery_required"),
            (TurnPhase::Unresolved, "unresolved"),
        ] {
            assert_eq!(phase.as_str(), label);
            assert_eq!(serde_json::to_value(phase).unwrap(), label);
        }
    }

    #[test]
    fn required_kinds_project_for_the_core_protocol() {
        let error: SessionObservation<Core> =
            SessionObservation::Row(TranscriptRow::error(NoticeScope::Host, "boom"));
        let line = project_host_event(&error, JsonlCorrelation::default()).unwrap();
        assert!(line.contains(r#""kind":"error""#), "{line}");
        assert_eq!(host_event_kind(&error), "row");
    }
}
