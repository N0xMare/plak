//! One content-free observation projection for local rows and exported span attributes.

use std::time::Duration;

use plakous::agent::{
    CompactionRefusalReason, DurabilityAttemptKind, DurabilityAttemptOutcome, TelemetryObservation,
    TelemetryObservationKind,
};
use plakous::model::Usage;
use plakous_hosted::product_observation::ChildKind;

pub(crate) mod labels;
#[cfg_attr(
    not(feature = "otel"),
    allow(
        dead_code,
        reason = "only the optional OTel bridge reads export-description fields"
    )
)]
mod span;
mod span_labels;
use labels::{
    compaction_outcome_text, compaction_reason_text, cost_fields_from_usage,
    credential_refresh_failure_text, credential_refresh_phase_text,
    credential_transport_phase_text, durability_attempt_kind_text, durability_attempt_outcome_text,
    id_text, row_duration_ns, row_timestamp_ns, terminal_status_text, usage_columns,
};
pub(crate) use span::{
    Attribute, AttributeValue, ParentSpec, Span, SpanIdentity, SpanKind, SpanSpec, SpanTiming,
    Status, SyntheticKind,
};
use span::{duration_ns, push_str, push_string, push_u64, push_u128};
use span_labels::{
    child_process_kind, compaction_outcome, compaction_reason, compaction_span_status,
    credential_refresh_span_status, durability_attempt, durability_attempt_span_status,
    durability_outcome, span_status, terminal_status,
};

#[derive(Clone, Debug)]
pub(crate) enum Cell {
    Null,
    Integer(i64),
    Text(String),
}

impl From<i64> for Cell {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}
impl From<u32> for Cell {
    fn from(value: u32) -> Self {
        Self::Integer(i64::from(value))
    }
}
impl From<bool> for Cell {
    fn from(value: bool) -> Self {
        Self::Integer(i64::from(value))
    }
}
impl From<String> for Cell {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}
impl From<&str> for Cell {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}
impl<T: Into<Self>> From<Option<T>> for Cell {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}
#[derive(Clone, Copy)]
pub(crate) enum RowKind {
    Job,
    TurnStarted,
    TurnFinished,
    ModelStarted,
    ModelFirstEvent,
    ModelStreamActivity,
    ModelFinished,
    ToolStarted,
    ToolFinished,
    CompactionStarted,
    CompactionFinished,
    UnpairedCompactionFinished,
    DurabilityFinished,
    DurabilityAttemptFinished,
    RetentionFinished,
    ChildProcessExited,
    PromptCacheDiagnosis,
    CredentialRefresh,
}

pub(crate) struct Row {
    pub kind: RowKind,
    pub values: Vec<Cell>,
    pub fallback: Option<Vec<Cell>>,
}

pub(crate) struct Projection {
    pub row: Option<Row>,
    pub span: Option<Span>,
    pub first_event: Option<Duration>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Output {
    Rows,
    Span,
}

pub(crate) fn project(observation: &TelemetryObservation, output: Output) -> Projection {
    let context = observation.context();
    let timestamp = row_timestamp_ns(observation.timestamp());
    let mut projection = Projection {
        row: None,
        span: None,
        first_event: None,
    };
    match observation.kind() {
        kind @ (TelemetryObservationKind::JobPromoted { .. }
        | TelemetryObservationKind::JobWaited { .. }
        | TelemetryObservationKind::JobPreempted { .. }
        | TelemetryObservationKind::JobNoticesDelivered { .. }) => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let (kind, tool, nested, source, hint, point, count) = match kind {
                    TelemetryObservationKind::JobPromoted {
                        tool,
                        nested,
                        hint_source,
                        hint_ms,
                    } => (
                        "promoted",
                        Some(tool.as_str()),
                        Some(nested),
                        Some(hint_source.as_str()),
                        Some(hint_ms),
                        None,
                        None,
                    ),
                    TelemetryObservationKind::JobWaited { tool } => {
                        ("waited", Some(tool.as_str()), None, None, None, None, None)
                    }
                    TelemetryObservationKind::JobPreempted { kind } => {
                        (kind.as_str(), None, None, None, None, None, None)
                    }
                    TelemetryObservationKind::JobNoticesDelivered { point, count } => (
                        "notices",
                        None,
                        None,
                        None,
                        None,
                        Some(point.as_str()),
                        Some(count),
                    ),
                    _ => unreachable!("only job observations reach this arm"),
                };
                Some(Row {
                    kind: RowKind::Job,
                    values: vec![
                        Cell::from(timestamp),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text)),
                        Cell::from(context.call_id().map(id_text)),
                        Cell::from(kind),
                        Cell::from(tool),
                        Cell::from(nested),
                        Cell::from(source),
                        Cell::from(hint.map(|value| i64::try_from(value).unwrap_or(i64::MAX))),
                        Cell::from(point),
                        Cell::from(count),
                    ],
                    fallback: None,
                })
            };
        }

        TelemetryObservationKind::TurnStarted { queue_time, origin } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let Some(turn_id) = context.turn_id() else {
                    break 'row None;
                };
                Some(Row {
                    kind: RowKind::TurnStarted,
                    values: vec![
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(id_text(turn_id)),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(queue_time)),
                        Cell::from(origin.as_str()),
                    ],
                    fallback: None,
                })
            };
        }

        TelemetryObservationKind::TurnFinished {
            elapsed,
            status,
            model_wall,
            tool_wall,
            compaction_wall,
            model_attempts,
            tool_calls,
            compactions,
            usage,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let Some(turn_id) = context.turn_id() else {
                    break 'row None;
                };
                Some(Row {
                    kind: RowKind::TurnFinished,
                    values: vec![
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(id_text(turn_id)),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from(terminal_status_text(status)),
                        Cell::from(row_duration_ns(model_wall)),
                        Cell::from(row_duration_ns(tool_wall)),
                        Cell::from(row_duration_ns(compaction_wall)),
                        Cell::from(i64::from(model_attempts)),
                        Cell::from(i64::from(tool_calls)),
                        Cell::from(i64::from(compactions)),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.span = 'span: {
                    let Some(turn_id) = context.turn_id() else {
                        break 'span None;
                    };
                    let mut attributes = Vec::new();
                    push_str(&mut attributes, "gen_ai.operation.name", "turn");
                    push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
                    push_str(&mut attributes, "plak.status", terminal_status(status));
                    push_u64(
                        &mut attributes,
                        "plak.turn.model_operations",
                        usage.model_operations(),
                    );
                    push_u64(
                        &mut attributes,
                        "plak.turn.model_operations_reporting_usage",
                        usage.model_operations_reporting_usage(),
                    );
                    push_u64(
                        &mut attributes,
                        "plak.turn.local_tool_attempts",
                        usage.local_tool_attempts(),
                    );
                    push_u128(
                        &mut attributes,
                        "plak.model_wall_ns",
                        duration_ns(model_wall),
                    );
                    push_u128(&mut attributes, "plak.tool_wall_ns", duration_ns(tool_wall));
                    push_u128(
                        &mut attributes,
                        "plak.compaction_wall_ns",
                        duration_ns(compaction_wall),
                    );
                    push_u64(
                        &mut attributes,
                        "plak.model_attempts",
                        u64::from(model_attempts),
                    );
                    push_u64(&mut attributes, "plak.tool_calls", u64::from(tool_calls));
                    push_u64(&mut attributes, "plak.compactions", u64::from(compactions));
                    Some(Span::Observed {
                        spec: SpanSpec {
                            name: "turn",
                            span_id: SpanIdentity::Turn(turn_id),
                            parent: ParentSpec::Root,
                            kind: SpanKind::Internal,
                            status: span_status(status),
                        },
                        timing: SpanTiming::Elapsed(elapsed),
                        attributes,
                        take_ttft: false,
                    })
                };
            }
        }

        TelemetryObservationKind::ModelStarted => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let (Some(turn_id), Some(attempt_id)) = (context.turn_id(), context.attempt_id())
                else {
                    break 'row None;
                };
                Some(Row {
                    kind: RowKind::ModelStarted,
                    values: vec![
                        Cell::from(id_text(attempt_id)),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(id_text(turn_id)),
                        Cell::from(timestamp),
                    ],
                    fallback: None,
                })
            };
        }

        TelemetryObservationKind::ModelFirstEvent { elapsed } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let Some(attempt_id) = context.attempt_id() else {
                    break 'row None;
                };
                Some(Row {
                    kind: RowKind::ModelFirstEvent,
                    values: vec![
                        Cell::from(id_text(attempt_id)),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text).unwrap_or_default()),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.first_event = Some(elapsed);
            }
        }

        TelemetryObservationKind::ModelStreamActivity { activity } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let Some(attempt_id) = context.attempt_id() else {
                    break 'row None;
                };
                let semantic_at = activity.last_semantic_event_elapsed_ms.map(|_| {
                    timestamp.saturating_sub(
                        i64::try_from(activity.semantic_gap_ms.saturating_mul(1_000_000))
                            .unwrap_or(i64::MAX),
                    )
                });
                Some(Row {
                    kind: RowKind::ModelStreamActivity,
                    values: vec![
                        Cell::from(id_text(attempt_id)),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text).unwrap_or_default()),
                        Cell::from(semantic_at),
                        Cell::from(i64::try_from(activity.keepalive_count).unwrap_or(i64::MAX)),
                        Cell::from(i64::try_from(activity.max_semantic_gap_ms).unwrap_or(i64::MAX)),
                    ],
                    fallback: None,
                })
            };
        }

        TelemetryObservationKind::ModelFinished {
            elapsed,
            status,
            usage,
            served_by,
            output_deltas,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let Some(attempt_id) = context.attempt_id() else {
                    break 'row None;
                };
                let (input, output, reasoning, cached_input, cache_write) = usage
                    .map(usage_columns)
                    .unwrap_or((None, None, None, None, None));
                let (cost_micros, cost_status) =
                    usage.map_or((None, "unknown"), cost_fields_from_usage);
                // Opaque observational token, not a provider or admission classification.
                let fingerprint = usage
                    .map(Usage::output_tokens)
                    .filter(|tokens| *tokens > 0)
                    .map(|tokens| {
                        let per_mille = u128::from(output_deltas) * 1000 / u128::from(tokens);
                        format!("dpt-v1:{per_mille}")
                    });
                let served_endpoint = served_by.map(|value| value.as_str().to_owned());
                Some(Row {
                    kind: RowKind::ModelFinished,
                    values: vec![
                        Cell::from(id_text(attempt_id)),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text).unwrap_or_default()),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from(terminal_status_text(status)),
                        Cell::from(input),
                        Cell::from(output),
                        Cell::from(reasoning),
                        Cell::from(cached_input),
                        Cell::from(cache_write),
                        Cell::from(served_endpoint),
                        Cell::from(cost_micros),
                        Cell::from(cost_status),
                        Cell::from(fingerprint),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.span = 'span: {
                    let Some(attempt_id) = context.attempt_id() else {
                        break 'span None;
                    };

                    let mut attributes = Vec::new();
                    push_str(&mut attributes, "gen_ai.operation.name", "model_attempt");
                    push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
                    push_str(&mut attributes, "plak.status", terminal_status(status));
                    if let Some(endpoint) = served_by {
                        attributes.push(Attribute::new(
                            "plak.served_endpoint",
                            AttributeValue::Text(endpoint.as_str().to_owned()),
                        ));
                    }
                    if let Some(model) = usage {
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.input_tokens",
                            model.input_tokens(),
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.output_tokens",
                            model.output_tokens(),
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.reasoning_tokens",
                            model.reasoning_tokens(),
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.cached_input_tokens",
                            model.cached_input_tokens(),
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.cache_write_tokens",
                            model.cache_write_tokens(),
                        );
                        if let Some(cost_micros) = model.cost_micros() {
                            push_u64(&mut attributes, "plak.cost_micros", cost_micros);
                        }
                    }

                    Some(Span::Observed {
                        spec: SpanSpec {
                            name: "model_attempt",
                            span_id: SpanIdentity::Attempt(attempt_id),
                            parent: ParentSpec::Turn,
                            kind: SpanKind::Client,
                            status: span_status(status),
                        },
                        timing: SpanTiming::Elapsed(elapsed),
                        attributes,
                        take_ttft: true,
                    })
                };
            }
        }

        TelemetryObservationKind::ToolStarted { tool } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let (Some(turn_id), Some(call_id)) = (context.turn_id(), context.call_id()) else {
                    break 'row None;
                };
                Some(Row {
                    kind: RowKind::ToolStarted,
                    values: vec![
                        Cell::from(id_text(call_id)),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(id_text(turn_id)),
                        Cell::from(context.attempt_id().map(id_text)),
                        Cell::from(timestamp),
                        Cell::from(tool.as_str()),
                    ],
                    fallback: None,
                })
            };
        }

        TelemetryObservationKind::ToolFinished {
            elapsed,
            status,
            output_bytes,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let Some(call_id) = context.call_id() else {
                    break 'row None;
                };
                Some(Row {
                    kind: RowKind::ToolFinished,
                    values: vec![
                        Cell::from(id_text(call_id)),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text).unwrap_or_default()),
                        Cell::from(context.attempt_id().map(id_text)),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from(terminal_status_text(status)),
                        Cell::from(i64::try_from(output_bytes).unwrap_or(i64::MAX)),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.span = 'span: {
                    let Some(call_id) = context.call_id() else {
                        break 'span None;
                    };
                    let mut attributes = Vec::new();
                    push_str(&mut attributes, "gen_ai.operation.name", "tool_call");
                    push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
                    push_str(&mut attributes, "plak.status", terminal_status(status));
                    push_u64(&mut attributes, "plak.output_bytes", output_bytes);
                    Some(Span::Observed {
                        spec: SpanSpec {
                            name: "tool_call",
                            span_id: SpanIdentity::Call(call_id),
                            parent: ParentSpec::Turn,
                            kind: SpanKind::Internal,
                            status: span_status(status),
                        },
                        timing: SpanTiming::Elapsed(elapsed),
                        attributes,
                        take_ttft: false,
                    })
                };
            }
        }

        TelemetryObservationKind::CompactionStarted { reason } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                // Pair Started→Finished on (session_id, branch_id, agent_id, turn_id-or-NULL):
                // update the most recent row with finished_at IS NULL; otherwise insert started-only.
                let turn_id = context.turn_id().map(id_text);
                Some(Row {
                    kind: RowKind::CompactionStarted,
                    values: vec![
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(turn_id),
                        Cell::from(timestamp),
                        Cell::from(compaction_reason_text(reason)),
                    ],
                    fallback: None,
                })
            };
        }

        TelemetryObservationKind::CompactionFinished {
            refusal,
            elapsed,
            outcome,
            reason,
            tokens_before,
            tokens_after,
            usage,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                let turn_id = context.turn_id().map(id_text);
                let (input, output, reasoning, cached_input, cache_write) = usage
                    .map(usage_columns)
                    .unwrap_or((None, None, None, None, None));
                let (cost_micros, cost_status) =
                    usage.map_or((None, "unknown"), cost_fields_from_usage);

                Some(Row {
                    kind: RowKind::CompactionFinished,
                    values: vec![
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from(compaction_outcome_text(outcome)),
                        Cell::from(compaction_reason_text(reason)),
                        Cell::from(tokens_before.map(i64::try_from).transpose().ok().flatten()),
                        Cell::from(tokens_after.map(i64::try_from).transpose().ok().flatten()),
                        Cell::from(input),
                        Cell::from(output),
                        Cell::from(reasoning),
                        Cell::from(cached_input),
                        Cell::from(cache_write),
                        Cell::from(cost_micros),
                        Cell::from(cost_status),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(turn_id.clone()),
                        Cell::from(refusal.map(CompactionRefusalReason::as_str)),
                    ],
                    fallback: Some(vec![
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(turn_id),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from(compaction_reason_text(reason)),
                        Cell::from(compaction_outcome_text(outcome)),
                        Cell::from(tokens_before.map(i64::try_from).transpose().ok().flatten()),
                        Cell::from(tokens_after.map(i64::try_from).transpose().ok().flatten()),
                        Cell::from(input),
                        Cell::from(output),
                        Cell::from(reasoning),
                        Cell::from(cached_input),
                        Cell::from(cache_write),
                        Cell::from(cost_micros),
                        Cell::from(cost_status),
                        Cell::from(refusal.map(CompactionRefusalReason::as_str)),
                    ]),
                })
            };
            if output == Output::Span {
                projection.span = {
                    let mut attributes = Vec::new();
                    push_str(&mut attributes, "gen_ai.operation.name", "compaction");
                    push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
                    push_str(
                        &mut attributes,
                        "plak.compaction.outcome",
                        compaction_outcome(outcome),
                    );
                    push_str(
                        &mut attributes,
                        "plak.compaction.reason",
                        compaction_reason(reason),
                    );
                    if let Some(refusal) = refusal {
                        push_str(
                            &mut attributes,
                            "plak.compaction.refusal_reason",
                            refusal.as_str(),
                        );
                    }
                    if let (Some(tokens_before), Some(tokens_after), Some(model)) =
                        (tokens_before, tokens_after, usage)
                    {
                        push_u64(
                            &mut attributes,
                            "plak.compaction.tokens_before",
                            tokens_before,
                        );
                        push_u64(
                            &mut attributes,
                            "plak.compaction.tokens_after",
                            tokens_after,
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.input_tokens",
                            model.input_tokens(),
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.output_tokens",
                            model.output_tokens(),
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.reasoning_tokens",
                            model.reasoning_tokens(),
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.cached_input_tokens",
                            model.cached_input_tokens(),
                        );
                        push_u64(
                            &mut attributes,
                            "gen_ai.usage.cache_write_tokens",
                            model.cache_write_tokens(),
                        );
                    }
                    Some(Span::Observed {
                        spec: SpanSpec {
                            name: "compaction",
                            span_id: SpanIdentity::Synthetic(SyntheticKind::Compaction),
                            parent: ParentSpec::TurnOrRoot,
                            kind: SpanKind::Internal,
                            status: compaction_span_status(outcome),
                        },
                        timing: SpanTiming::Elapsed(elapsed),
                        attributes,
                        take_ttft: false,
                    })
                };
            }
        }

        TelemetryObservationKind::DurabilityFinished { elapsed, status } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                Some(Row {
                    kind: RowKind::DurabilityFinished,
                    values: vec![
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text)),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from("durability_finished"),
                        Cell::from(terminal_status_text(status)),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.span = {
                    let mut attributes = Vec::new();
                    push_str(&mut attributes, "gen_ai.operation.name", "durability");
                    push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
                    push_str(&mut attributes, "plak.status", terminal_status(status));
                    Some(Span::Observed {
                        spec: SpanSpec {
                            name: "durability",
                            span_id: SpanIdentity::Synthetic(SyntheticKind::Durability),
                            parent: ParentSpec::TurnOrRoot,
                            kind: SpanKind::Internal,
                            status: span_status(status),
                        },
                        timing: SpanTiming::Elapsed(elapsed),
                        attributes,
                        take_ttft: false,
                    })
                };
            }
        }

        TelemetryObservationKind::DurabilityAttemptFinished {
            elapsed,
            attempt,
            outcome,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                if attempt == DurabilityAttemptKind::InitialCommit
                    && outcome == DurabilityAttemptOutcome::Committed
                {
                    break 'row None;
                }
                Some(Row {
                    kind: RowKind::DurabilityAttemptFinished,
                    values: vec![
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text)),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from("durability_attempt_finished"),
                        Cell::from(durability_attempt_kind_text(attempt)),
                        Cell::from(durability_attempt_outcome_text(outcome)),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.span = {
                    let mut attributes = Vec::new();
                    push_str(
                        &mut attributes,
                        "gen_ai.operation.name",
                        "durability_attempt",
                    );
                    push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
                    push_str(
                        &mut attributes,
                        "plak.durability.attempt",
                        durability_attempt(attempt),
                    );
                    push_str(
                        &mut attributes,
                        "plak.durability.outcome",
                        durability_outcome(outcome),
                    );
                    Some(Span::Observed {
                        spec: SpanSpec {
                            name: "durability_attempt",
                            span_id: SpanIdentity::Synthetic(SyntheticKind::DurabilityAttempt),
                            parent: ParentSpec::TurnOrRoot,
                            kind: SpanKind::Internal,
                            status: durability_attempt_span_status(outcome),
                        },
                        timing: SpanTiming::Elapsed(elapsed),
                        attributes,
                        take_ttft: false,
                    })
                };
            }
        }

        TelemetryObservationKind::RetentionFinished {
            elapsed,
            status,
            pruned_checkpoints,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                Some(Row {
                    kind: RowKind::RetentionFinished,
                    values: vec![
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text)),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from(terminal_status_text(status)),
                        Cell::from(i64::try_from(pruned_checkpoints).unwrap_or(i64::MAX)),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.span = {
                    let mut attributes = Vec::new();
                    push_str(&mut attributes, "gen_ai.operation.name", "retention");
                    push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
                    push_str(&mut attributes, "plak.status", terminal_status(status));
                    push_u64(
                        &mut attributes,
                        "plak.retention.pruned_checkpoints",
                        pruned_checkpoints,
                    );
                    Some(Span::Observed {
                        spec: SpanSpec {
                            name: "retention",
                            span_id: SpanIdentity::Synthetic(SyntheticKind::Retention),
                            parent: ParentSpec::TurnOrRoot,
                            kind: SpanKind::Internal,
                            status: span_status(status),
                        },
                        timing: SpanTiming::Elapsed(elapsed),
                        attributes,
                        take_ttft: false,
                    })
                };
            }
        }

        TelemetryObservationKind::ChildProcessExited {
            child,
            user_cpu,
            system_cpu,
            peak_rss_bytes,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                Some(Row {
                    kind: RowKind::ChildProcessExited,
                    values: vec![
                        Cell::from(timestamp),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from("child_exited"),
                        Cell::from(ChildKind::from_agent_child(child).as_str()),
                        Cell::from(None::<i64>),
                        Cell::from(peak_rss_bytes.map(i64::try_from).transpose().ok().flatten()),
                        Cell::from(row_duration_ns(user_cpu)),
                        Cell::from(row_duration_ns(system_cpu)),
                        Cell::from(None::<i64>),
                        Cell::from(None::<i64>),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.span = {
                    let mut attributes = Vec::new();
                    push_str(
                        &mut attributes,
                        "gen_ai.operation.name",
                        "child_process_exited",
                    );
                    push_str(
                        &mut attributes,
                        "plak.child_kind",
                        child_process_kind(child),
                    );
                    push_u128(&mut attributes, "plak.user_cpu_ns", duration_ns(user_cpu));
                    push_u128(
                        &mut attributes,
                        "plak.system_cpu_ns",
                        duration_ns(system_cpu),
                    );
                    if let Some(peak_rss_bytes) = peak_rss_bytes {
                        push_u64(&mut attributes, "plak.peak_rss_bytes", peak_rss_bytes);
                    }

                    Some(Span::Observed {
                        spec: SpanSpec {
                            name: "child_process_exited",
                            span_id: SpanIdentity::Synthetic(SyntheticKind::ChildProcessExited),
                            parent: ParentSpec::TurnOrRoot,
                            kind: SpanKind::Internal,
                            status: Status::Ok,
                        },
                        timing: SpanTiming::Instant,
                        attributes,
                        take_ttft: false,
                    })
                };
            }
        }

        TelemetryObservationKind::PromptCacheDiagnosis {
            reason,
            missed_input_tokens,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                Some(Row {
                    kind: RowKind::PromptCacheDiagnosis,
                    values: vec![
                        Cell::from(timestamp),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map(id_text)),
                        Cell::from(context.attempt_id().map(id_text)),
                        Cell::from(reason.as_str()),
                        Cell::from(
                            missed_input_tokens
                                .map(|tokens| i64::try_from(tokens).unwrap_or(i64::MAX)),
                        ),
                    ],
                    fallback: None,
                })
            };
        }

        TelemetryObservationKind::CredentialRefresh {
            provider,
            phase,
            failure_class,
            transport_phase,
            http_status,
            elapsed,
            generation,
            detail,
        } => {
            projection.row = 'row: {
                if output != Output::Rows {
                    break 'row None;
                }
                Some(Row {
                    kind: RowKind::CredentialRefresh,
                    values: vec![
                        Cell::from(timestamp),
                        Cell::from(provider.as_str()),
                        Cell::from(credential_refresh_phase_text(phase)),
                        Cell::from(failure_class.map(credential_refresh_failure_text)),
                        Cell::from(http_status.map(i64::from)),
                        Cell::from(elapsed.map(row_duration_ns)),
                        Cell::from(i64::try_from(generation).unwrap_or(i64::MAX)),
                        Cell::from(transport_phase.map(credential_transport_phase_text)),
                        Cell::from(
                            detail
                                .reason
                                .map(plakous::agent::CredentialRefreshFailureReason::as_str),
                        ),
                        Cell::from(detail.oauth_error.code()),
                        Cell::from(detail.oauth_error.description()),
                    ],
                    fallback: None,
                })
            };
            if output == Output::Span {
                projection.span = {
                    let timestamp = observation.timestamp();

                    let mut attributes = vec![
                        Attribute::new(
                            "plak.timestamp_secs",
                            AttributeValue::Integer(
                                i64::try_from(timestamp.seconds()).unwrap_or(i64::MAX),
                            ),
                        ),
                        Attribute::new(
                            "plak.timestamp_nanos",
                            AttributeValue::Integer(i64::from(timestamp.nanoseconds())),
                        ),
                    ];
                    push_str(
                        &mut attributes,
                        "gen_ai.operation.name",
                        "credential_refresh",
                    );
                    push_string(
                        &mut attributes,
                        "plak.credential.provider",
                        provider.as_str().to_owned(),
                    );
                    push_str(
                        &mut attributes,
                        "plak.credential.phase",
                        credential_refresh_phase_text(phase),
                    );
                    if let Some(failure_class) = failure_class {
                        push_str(
                            &mut attributes,
                            "plak.credential.failure_class",
                            credential_refresh_failure_text(failure_class),
                        );
                    }
                    if let Some(transport_phase) = transport_phase {
                        push_str(
                            &mut attributes,
                            "plak.credential.transport_phase",
                            credential_transport_phase_text(transport_phase),
                        );
                    }
                    if let Some(http_status) = http_status {
                        push_u64(
                            &mut attributes,
                            "plak.credential.http_status",
                            u64::from(http_status),
                        );
                    }
                    if let Some(reason) = detail.reason {
                        push_str(
                            &mut attributes,
                            "plak.credential.failure_reason",
                            reason.as_str(),
                        );
                    }
                    push_u64(&mut attributes, "plak.credential.generation", generation);
                    if let Some(elapsed) = elapsed {
                        push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
                    }
                    Some(Span::Credential {
                        provider: provider.as_str().to_owned(),
                        generation,
                        elapsed,
                        status: credential_refresh_span_status(phase),
                        attributes,
                    })
                };
            }
        }
        TelemetryObservationKind::ModelReresolved { .. }
        | TelemetryObservationKind::ModelArtifactPrefixDrift { .. } => {}
        _ => {}
    }
    projection
}
