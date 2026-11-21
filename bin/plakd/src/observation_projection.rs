//! Content-free SQLite rows projected from telemetry observations.

use plakous::agent::{
    CompactionRefusalReason, DurabilityAttemptKind, DurabilityAttemptOutcome, TelemetryContext,
    TelemetryObservation, TelemetryObservationKind as Kind,
};
use plakous::model::Usage;
use plakous_hosted::product_observation::ChildKind;

pub(crate) mod labels;
use labels::{
    compaction_outcome_text, compaction_reason_text, cost_fields_from_usage,
    credential_refresh_failure_text, credential_refresh_phase_text,
    credential_transport_phase_text, durability_attempt_kind_text, durability_attempt_outcome_text,
    id_text, row_duration_ns, row_timestamp_ns, terminal_status_text, usage_columns,
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
    ToolObservationClipped,
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

fn token_cell(value: Option<u64>) -> Option<i64> {
    value.map(|tokens| i64::try_from(tokens).unwrap_or(i64::MAX))
}

fn digest_hex(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

pub(crate) fn project(observation: &TelemetryObservation) -> Option<Row> {
    let projection = Projection {
        context: observation.context(),
        timestamp: row_timestamp_ns(observation.timestamp()),
    };
    let kind = observation.kind();
    match kind {
        Kind::JobPromoted { .. }
        | Kind::JobWaited { .. }
        | Kind::JobPreempted { .. }
        | Kind::JobNoticesDelivered { .. } => Some(projection.job(&kind)),
        Kind::TurnStarted { .. } | Kind::TurnFinished { .. } => projection.turn(&kind),
        Kind::ModelStarted
        | Kind::ModelFirstEvent { .. }
        | Kind::ModelStreamActivity { .. }
        | Kind::PromptCacheDiagnosis { .. } => projection.model_progress(&kind),
        Kind::ModelFinished { .. } => projection.model_finished(&kind),
        Kind::ToolStarted { .. }
        | Kind::ToolFinished { .. }
        | Kind::ToolObservationClipped { .. } => projection.tool(&kind),
        Kind::CompactionStarted { .. } | Kind::CompactionFinished { .. } => {
            Some(projection.compaction(&kind))
        }
        Kind::DurabilityFinished { .. }
        | Kind::DurabilityAttemptFinished { .. }
        | Kind::RetentionFinished { .. } => projection.durability(&kind),
        Kind::ChildProcessExited { .. } => Some(projection.process(&kind)),
        Kind::CredentialRefresh { .. } => Some(projection.credential(&kind)),
        _ => None,
    }
}

struct Projection {
    context: TelemetryContext,
    timestamp: i64,
}

impl Projection {
    fn job(self, kind: &Kind) -> Row {
        let Self { context, timestamp } = self;
        let (kind, tool, nested, source, hint, point, count) = match *kind {
            Kind::JobPromoted {
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
            Kind::JobWaited { tool } => {
                ("waited", Some(tool.as_str()), None, None, None, None, None)
            }
            Kind::JobPreempted { kind } => (kind.as_str(), None, None, None, None, None, None),
            Kind::JobNoticesDelivered { point, count } => (
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
        Row {
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
        }
    }

    fn turn(self, kind: &Kind) -> Option<Row> {
        let Self { context, timestamp } = self;
        match *kind {
            Kind::TurnStarted { queue_time, origin } => {
                let turn_id = context.turn_id()?;
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
            }

            Kind::TurnFinished {
                elapsed,
                status,
                model_wall,
                tool_wall,
                compaction_wall,
                model_attempts,
                tool_calls,
                compactions,
                prepared_input_high_water,
                usage: _,
            } => {
                let turn_id = context.turn_id()?;
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
                        Cell::from(token_cell(prepared_input_high_water)),
                    ],
                    fallback: None,
                })
            }
            _ => unreachable!("only turn observations reach this projection"),
        }
    }

    fn model_progress(self, kind: &Kind) -> Option<Row> {
        let Self { context, timestamp } = self;
        match *kind {
            Kind::ModelStarted => {
                let (Some(turn_id), Some(attempt_id)) = (context.turn_id(), context.attempt_id())
                else {
                    return None;
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
            }

            Kind::ModelFirstEvent { elapsed } => {
                let attempt_id = context.attempt_id()?;
                Some(Row {
                    kind: RowKind::ModelFirstEvent,
                    values: vec![
                        Cell::from(id_text(attempt_id)),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map_or_default(id_text)),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                    ],
                    fallback: None,
                })
            }

            Kind::ModelStreamActivity { activity } => {
                let attempt_id = context.attempt_id()?;
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
                        Cell::from(context.turn_id().map_or_default(id_text)),
                        Cell::from(semantic_at),
                        Cell::from(i64::try_from(activity.keepalive_count).unwrap_or(i64::MAX)),
                        Cell::from(i64::try_from(activity.max_semantic_gap_ms).unwrap_or(i64::MAX)),
                    ],
                    fallback: None,
                })
            }

            Kind::PromptCacheDiagnosis {
                reason,
                missed_input_tokens,
            } => Some(Row {
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
                        missed_input_tokens.map(|tokens| i64::try_from(tokens).unwrap_or(i64::MAX)),
                    ),
                ],
                fallback: None,
            }),
            _ => unreachable!("only model progress observations reach this projection"),
        }
    }

    fn model_finished(self, kind: &Kind) -> Option<Row> {
        let Self { context, timestamp } = self;
        match *kind {
            Kind::ModelFinished {
                elapsed,
                status,
                usage,
                served_by,
                output_deltas,
                measurement,
            } => {
                let attempt_id = context.attempt_id()?;
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
                        Cell::from(context.turn_id().map_or_default(id_text)),
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
                        Cell::from(token_cell(measurement.context_window_tokens)),
                        Cell::from(token_cell(measurement.instruction_bytes)),
                        Cell::from(token_cell(measurement.tool_schema_bytes)),
                        Cell::from(token_cell(measurement.prepared_input_tokens)),
                        Cell::from(measurement.idle_gap.map(row_duration_ns)),
                    ],
                    fallback: None,
                })
            }
            _ => unreachable!("only model finished observations reach this projection"),
        }
    }

    fn tool(self, kind: &Kind) -> Option<Row> {
        let Self { context, timestamp } = self;
        match *kind {
            Kind::ToolStarted { tool } => {
                let (Some(turn_id), Some(call_id)) = (context.turn_id(), context.call_id()) else {
                    return None;
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
            }

            Kind::ToolFinished {
                elapsed,
                status,
                output_bytes,
                measurement,
            } => {
                let call_id = context.call_id()?;
                Some(Row {
                    kind: RowKind::ToolFinished,
                    values: vec![
                        Cell::from(id_text(call_id)),
                        Cell::from(id_text(context.session_id())),
                        Cell::from(id_text(context.branch_id())),
                        Cell::from(id_text(context.run_id())),
                        Cell::from(id_text(context.agent_id())),
                        Cell::from(context.turn_id().map_or_default(id_text)),
                        Cell::from(context.attempt_id().map(id_text)),
                        Cell::from(timestamp),
                        Cell::from(row_duration_ns(elapsed)),
                        Cell::from(terminal_status_text(status)),
                        Cell::from(i64::try_from(output_bytes).unwrap_or(i64::MAX)),
                        Cell::from(measurement.access_digest.map(digest_hex)),
                        Cell::from(token_cell(measurement.range_first_line)),
                        Cell::from(token_cell(measurement.range_lines)),
                        Cell::from(measurement.page_partial),
                        Cell::from(
                            measurement
                                .command_class
                                .map(|name| name.as_str().to_owned()),
                        ),
                        Cell::from(measurement.wait_count),
                        Cell::from(
                            measurement
                                .wait_kind
                                .map(plakous::agent::WaitTargetKind::as_str),
                        ),
                        Cell::from(
                            measurement
                                .wait_outcome
                                .map(plakous::agent::WaitOutcome::as_str),
                        ),
                    ],
                    fallback: None,
                })
            }

            Kind::ToolObservationClipped { bytes_dropped, .. } => {
                let (Some(turn_id), Some(call_id)) = (context.turn_id(), context.call_id()) else {
                    return None;
                };
                Some(Row {
                    kind: RowKind::ToolObservationClipped,
                    values: vec![
                        Cell::from(id_text(turn_id)),
                        Cell::from(id_text(call_id)),
                        Cell::from(i64::try_from(bytes_dropped).unwrap_or(i64::MAX)),
                    ],
                    fallback: None,
                })
            }
            _ => unreachable!("only tool observations reach this projection"),
        }
    }

    fn compaction(self, kind: &Kind) -> Row {
        let Self { context, timestamp } = self;
        match *kind {
            Kind::CompactionStarted { reason } => {
                // Pair Started→Finished on (session_id, branch_id, agent_id, turn_id-or-NULL):
                // update the most recent row with finished_at IS NULL; otherwise insert started-only.
                let turn_id = context.turn_id().map(id_text);
                Row {
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
                }
            }

            Kind::CompactionFinished {
                refusal,
                elapsed,
                outcome,
                reason,
                tokens_before,
                tokens_after,
                usage,
                measurement,
            } => {
                let turn_id = context.turn_id().map(id_text);
                let (input, output, reasoning, cached_input, cache_write) = usage
                    .map(usage_columns)
                    .unwrap_or((None, None, None, None, None));
                let (cost_micros, cost_status) =
                    usage.map_or((None, "unknown"), cost_fields_from_usage);

                Row {
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
                        Cell::from(token_cell(measurement.context_window_tokens)),
                        Cell::from(token_cell(measurement.threshold_tokens)),
                        Cell::from(token_cell(measurement.occupancy_tokens)),
                        Cell::from(measurement.idle_gap.map(row_duration_ns)),
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
                        Cell::from(token_cell(measurement.context_window_tokens)),
                        Cell::from(token_cell(measurement.threshold_tokens)),
                        Cell::from(token_cell(measurement.occupancy_tokens)),
                        Cell::from(measurement.idle_gap.map(row_duration_ns)),
                    ]),
                }
            }
            _ => unreachable!("only compaction observations reach this projection"),
        }
    }

    fn durability(self, kind: &Kind) -> Option<Row> {
        let Self { context, timestamp } = self;
        match *kind {
            Kind::DurabilityFinished { elapsed, status } => Some(Row {
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
            }),

            Kind::DurabilityAttemptFinished {
                elapsed,
                attempt,
                outcome,
            } => {
                if attempt == DurabilityAttemptKind::InitialCommit
                    && outcome == DurabilityAttemptOutcome::Committed
                {
                    return None;
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
            }

            Kind::RetentionFinished {
                elapsed,
                status,
                pruned_checkpoints,
            } => Some(Row {
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
            }),
            _ => unreachable!("only durability observations reach this projection"),
        }
    }

    fn process(self, kind: &Kind) -> Row {
        let Self { context, timestamp } = self;
        match *kind {
            Kind::ChildProcessExited {
                child,
                user_cpu,
                system_cpu,
                peak_rss_bytes,
            } => Row {
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
            },
            _ => unreachable!("only process observations reach this projection"),
        }
    }

    fn credential(self, kind: &Kind) -> Row {
        let timestamp = self.timestamp;
        match *kind {
            Kind::CredentialRefresh {
                provider,
                phase,
                failure_class,
                transport_phase,
                http_status,
                elapsed,
                generation,
                detail,
            } => Row {
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
                    Cell::from(
                        detail
                            .account
                            .as_ref()
                            .map(plakous::agent::CredentialAccountLabel::as_str),
                    ),
                ],
                fallback: None,
            },
            _ => unreachable!("only credential observations reach this projection"),
        }
    }
}
