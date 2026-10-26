//! Tool lifecycle events and presentation facts.

use super::compose::{
    compose_cancelled_state, compose_counts_label, compose_program_source, emit_compose_update,
    observe_compose_subject, observe_settled_compose_marker,
};
use super::{EventApplier, event_timestamp_ms, timestamp_ms};
use crate::runtime::admission::commit_session_update;
use crate::runtime::logging::log_event;
use crate::runtime::tool_facts::{wait_settled, wait_started};
use crate::runtime::tree::{
    continuation_parent_call_id, emit_tree_snapshot_if_revision_advanced, lineage_parent_call_id,
    nested_parent_call_id,
};
use crate::runtime::turn::flush_assistant_delta;
use crate::runtime::{HostEvent, Sender, TurnProjection};
use crate::session_log::{
    SessionEvent, redact_text_preview, tool_rejected_reason, tool_rejected_record,
};
use crate::sessions;
use crate::sessions::{SessionRecord, TranscriptRow, live_tool_subject};
use plakous::agent::{AgentEventKind, ToolCallObservation};
use std::collections::HashMap;

/// Update one live activity row. No store is involved.
pub(super) fn observe_tool_activity(session: &mut SessionRecord, activity: sessions::ToolItem) {
    session.upsert_tool_activity(activity);
}

pub(in crate::runtime) fn observe_cancelled_tool_activity<
    P: plakous_host_protocol::ProtocolExtension,
>(
    session: &mut SessionRecord,
    events: &Sender<P>,
    projection: Option<&TurnProjection<P>>,
) {
    observe_terminal_tool_activity(session, events, "cancelled", projection);
}

pub(in crate::runtime) fn observe_terminal_tool_activity<
    P: plakous_host_protocol::ProtocolExtension,
>(
    session: &mut SessionRecord,
    events: &Sender<P>,
    state: &str,
    projection: Option<&TurnProjection<P>>,
) {
    let running = session
        .transcript
        .iter()
        .filter_map(|row| row.as_tool().cloned())
        .filter(|activity| activity.state == "running")
        .collect::<Vec<_>>();
    let closed_at_ms = timestamp_ms(crate::product_observation::wall_timestamp());
    for mut activity in running {
        state.clone_into(&mut activity.state);
        // The sweep ends the call, so the row records when, as every other
        // terminal does: a group's elapsed span reads it live and restored.
        activity.finished_at_ms = Some(closed_at_ms);
        // carried on every line the host can stamp. The sweep can
        // — the row it is closing holds the call's start.
        let duration_ms = activity
            .started_at_ms
            .and_then(|started| closed_at_ms.checked_sub(started));
        observe_tool_activity(session, activity.clone());
        let compose = projection.and_then(|projection| projection.cells.get(&activity.call_id));
        // a cell that terminalises with accepted requests that
        // never dispatched states the remainder it *observed*, or no ratio.
        let line_state = match compose {
            Some(compose) if state == "cancelled" => compose_cancelled_state(compose),
            _ => state.to_owned(),
        };
        let _ = events.send(HostEvent::ToolLine {
            call_id: Some(activity.call_id),
            // Persisted lineage, so a gap-closed child still folds under its
            // cell instead of surfacing as a stray flat row.
            parent_call_id: activity.parent_call_id,
            cell_id: activity
                .cell_id
                .or_else(|| compose.and_then(|compose| compose.cell_id.clone())),
            kind: compose.map_or_else(
                || crate::activity::tool_activity_kind(&activity.name),
                |_| plakous_host_protocol::views::presentation::ActivityKind::Compose,
            ),
            name: activity.name,
            state: line_state,
            subject: compose.map_or(activity.subject, |compose| {
                Some(compose_counts_label(
                    compose,
                    projection.is_some_and(|projection| projection.gap_observed),
                ))
            }),
            detail: activity.detail,
            duration_ms,
            started_at_ms: activity.started_at_ms,
            finished_at_ms: activity.finished_at_ms,
            wait: None,
        });
    }
}

/// Wall time of one call from the `started_at_ms` the host persisted for it.
///
/// This is the same pair of stamps a resumed transcript derives its duration
/// from (`ActivityItem::with_derived_duration`), so a live row and the row a
/// `/resume` rebuilds report the same number for the same call. A cell that
/// yields is the case that made the difference visible: its marker
/// terminalises, reopens on the next dispatch and terminalises again, and only
/// the first stamp describes when the cell started.
fn observed_call_duration_ms(
    session: &SessionRecord,
    call_id: &str,
    observed_at_ms: u64,
) -> Option<u64> {
    session
        .transcript
        .tool_activity(call_id)
        .and_then(|activity| activity.started_at_ms)
        .map(|started_at_ms| observed_at_ms.saturating_sub(started_at_ms))
}

/// What a process-shaped call's subject is read against.
pub(in crate::runtime) struct ProcessContext<'a> {
    pub(in crate::runtime) label: Option<&'static str>,
    pub(in crate::runtime) args: Option<&'a serde_json::Value>,
    pub(in crate::runtime) locator: Option<plakous::sessions::SessionLocator>,
    pub(in crate::runtime) processes: &'a [plakous_host_protocol::views::ProcessView],
    pub(in crate::runtime) commands: &'a HashMap<String, String>,
}

/// A starting tool row's subject and detail.
pub(in crate::runtime) struct StartedToolText {
    pub(in crate::runtime) subject: Option<String>,
    pub(in crate::runtime) detail: Option<String>,
}

/// The subject and detail a tool row starts with, from the facts its start
/// carries. The root's live path and an agent transcript read both call this,
/// so a call reads the same in either.
pub(in crate::runtime) fn started_tool_text(
    name: &str,
    exposed_name: Option<&str>,
    observation: Option<&ToolCallObservation>,
    info: Option<&crate::runtime::PendingToolInfo>,
    process: ProcessContext<'_>,
) -> StartedToolText {
    let kind = crate::activity::tool_activity_kind(name);
    // A Compose head's subject is its host-counted tally (§5.8); the
    // program source is its detail, never its subject.
    let subject = (kind != plakous_host_protocol::views::presentation::ActivityKind::Compose)
        .then(|| crate::runtime::tools::started_tool_subject(name, exposed_name, observation, info))
        .flatten()
        .map(|value| live_tool_subject(&value));
    let subject = process
        .label
        .zip(process.args)
        .map(|(label, args)| {
            let subject = if label == "Shell" {
                crate::runtime::tools::process_subject(
                    label,
                    args,
                    process.locator,
                    process.processes,
                    None,
                )
            } else {
                crate::runtime::tools::process_subject_for_ids(
                    label,
                    &crate::runtime::tools::process_ids(args),
                    args.get("input")
                        .or_else(|| args.get("chars"))
                        .and_then(serde_json::Value::as_str),
                    process.locator,
                    process.processes,
                    None,
                    process.commands,
                )
            };
            crate::runtime::tools::process_action_subject(label, &subject, false)
        })
        .or(subject);
    // §5.8: a Compose head's detail is the program itself, so the
    // expanded rail shows the code the cell ran rather than its JSON
    // encoding.
    let detail = if name == "run_py" {
        compose_program_source(observation.map(ToolCallObservation::arguments_preview)).map(
            |source| {
                let source = redact_text_preview(&source);
                let fence = "`".repeat(
                    source
                        .split(|c| c != '`')
                        .map(str::len)
                        .max()
                        .unwrap_or(0)
                        .max(2)
                        + 1,
                );
                format!("{fence}python\n{source}\n{fence}")
            },
        )
    } else {
        (kind == plakous_host_protocol::views::presentation::ActivityKind::Compose)
            .then(|| {
                compose_program_source(observation.map(ToolCallObservation::arguments_preview))
                    .map(|program| redact_text_preview(&program))
            })
            .flatten()
    }
    .or_else(|| {
        observation.and_then(|item| {
            crate::activity::tool_detail_text(
                Some(&redact_text_preview(item.arguments_preview())),
                None,
            )
        })
    })
    .or_else(|| info.map(|item| item.detail.clone()));
    StartedToolText { subject, detail }
}

/// A settled process wait's subject, `Waited on process <ids> · <outcome>`,
/// from the ids its arguments named and its result preview. The root's live
/// path and an agent transcript read both call this.
pub(in crate::runtime) fn settled_process_wait_subject(
    ids: &[String],
    preview: Option<&str>,
    locator: Option<plakous::sessions::SessionLocator>,
    processes: &[plakous_host_protocol::views::ProcessView],
    commands: &HashMap<String, String>,
) -> String {
    let result = preview
        .and_then(|preview| serde_json::from_str::<serde_json::Value>(preview).ok())
        .unwrap_or(serde_json::Value::Null);
    let subject = crate::runtime::tools::process_subject_for_ids(
        "process_session_wait",
        ids,
        None,
        locator,
        processes,
        Some(&result),
        commands,
    );
    crate::runtime::tools::process_action_subject("process_session_wait", &subject, true)
}

/// A completed call's state label: `failed` when the call failed, and for a
/// process call that succeeded, how the command it ran ended (C8).
pub(crate) fn completed_tool_state(name: &str, result: Option<(bool, &str)>) -> String {
    match result {
        Some((false, _)) => "failed".to_owned(),
        Some((true, preview))
            if crate::activity::tool_activity_kind(name)
                == plakous_host_protocol::views::presentation::ActivityKind::Process =>
        {
            crate::activity::process_failure(preview)
                .map_or_else(|| "succeeded".to_owned(), |how| format!("failed:{how}"))
        }
        _ => "succeeded".to_owned(),
    }
}

/// A completed call's detail: its start detail joined with its redacted
/// result preview. Spec 02 §5.4: readable text, never `Arguments:` / `Result:`
/// JSON dumps.
pub(crate) fn completed_tool_detail(
    started_detail: Option<&str>,
    preview: Option<&str>,
) -> Option<String> {
    preview.and_then(|preview| {
        crate::activity::tool_detail_text(started_detail, Some(&redact_text_preview(preview)))
    })
}

/// §5.2: a completed header carries the host's own result-derived counts
/// after its start subject. Only the host parses tool output for them.
pub(in crate::runtime) fn completed_tool_summary(
    name: &str,
    started_subject: Option<&str>,
    preview: Option<&str>,
) -> Option<String> {
    let preview = preview?;
    let subject = started_subject?;
    let summary = crate::runtime::tools::tool_result_summary(name, Some(preview))?;
    Some(format!("{subject} {summary}"))
}

impl<P: plakous_host_protocol::ProtocolExtension> EventApplier<'_, P> {
    /// A started tool call: its live row, subject and process facts.
    pub(super) fn tool_started(self) {
        let Self {
            event,
            call_id_str,
            roots,
            session,
            events,
            delta_buf,
            last_flush,
            tool_subjects,
            projection,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::ToolStarted {
            identity,
            lineage,
            observation,
        } = event.kind()
        else {
            return;
        };
        flush_assistant_delta(delta_buf, last_flush, events, true);
        let canonical = identity.canonical_name().as_str().to_owned();
        let exposed_name = identity
            .exposed_name()
            .map(plakous::model::ToolName::as_str);
        let info = tool_subjects.take(&canonical, call_id_str.as_deref());
        let process_args = observation.as_ref().and_then(|item| {
            serde_json::from_str::<serde_json::Value>(item.arguments_preview()).ok()
        });
        // A Grok output or kill call naming a child reads as the await or
        // the interrupt it is, from here on (presentation only).
        let name = presented_start_name(
            projection,
            call_id_str.as_deref(),
            &canonical,
            process_args.as_ref(),
            plakous_host_protocol::topology::is_topology_registration(
                identity.registered_tool_id().as_str(),
            ),
        );
        // Once presentation renamed the call, the model-facing name is the
        // process tool it replaced. The restored row has no exposed name;
        // the live row must not keep one either, or the subject is a
        // process wait over an id the process tools cannot parse.
        let label_exposed = (name == canonical).then_some(exposed_name).flatten();
        let process_label = process_args.as_ref().and_then(|args| {
            crate::runtime::tools::process_observation_label(&name, label_exposed, args)
        });
        let wait = wait_started(&name, process_label, process_args.as_ref());
        if let Some(call_id) = call_id_str.as_ref()
            && let Some(candidate) =
                crate::runtime::tool_facts::adopt_candidate_arg(&name, process_args.as_ref())
        {
            projection
                .adopt_candidates
                .insert(call_id.clone(), candidate);
        }
        let kind = crate::activity::tool_activity_kind(&name);
        let processes = process_label
            .map(|_| roots.job_registry().snapshot())
            .unwrap_or_default();
        if let (Some(args), Some(locator)) = (
            process_label.and(process_args.as_ref()),
            session.plakous_locator,
        ) {
            for id in &crate::runtime::tools::process_ids(args) {
                let key = crate::runtime::tools::process_key(locator, id);
                if let Some(process) = processes
                    .iter()
                    .find(|process| process.id == key && !process.command.is_empty())
                {
                    projection
                        .process_commands
                        .insert(key, process.command.clone());
                }
            }
        }
        let StartedToolText { subject, detail } = started_tool_text(
            &name,
            label_exposed,
            observation.as_ref(),
            info.as_ref(),
            ProcessContext {
                label: process_label,
                args: process_args.as_ref(),
                locator: session.plakous_locator,
                processes: &processes,
                commands: &projection.process_commands,
            },
        );
        if process_label == Some("process_session_wait")
            && let Some(id) = call_id_str.as_ref()
            && let Some(args) = process_args.as_ref()
        {
            projection
                .process_waits
                .insert(id.clone(), crate::runtime::tools::process_ids(args));
        }
        log_event(
            log,
            observed_session_id,
            SessionEvent::new("tool.started")
                .str("name", &name)
                .opt_str(
                    "exposed_name",
                    identity
                        .exposed_name()
                        .map(plakous::model::ToolName::as_str),
                )
                .opt_str("call_id", call_id_str.as_deref())
                .opt_str("subject", subject.as_deref())
                .opt_str("arguments", detail.as_deref()),
        );
        let nested_parent = nested_parent_call_id(lineage, projection);
        let parent_call_id = nested_parent
            .clone()
            .or_else(|| continuation_parent_call_id(&name, lineage, projection));
        let cell_id = lineage.cell_id().map(str::to_owned);
        if let Some(call_id) = call_id_str.as_ref() {
            observe_tool_activity(
                session,
                sessions::ToolItem {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    exposed_name: row_exposed_name(
                        identity.canonical_name(),
                        identity.exposed_name(),
                        &name,
                    ),
                    kind: crate::activity::tool_activity_kind(&name),
                    state: "running".into(),
                    subject: subject.clone(),
                    detail: detail.clone(),
                    started_at_ms: Some(event_timestamp_ms(event)),
                    finished_at_ms: None,
                    parent_call_id: parent_call_id.clone(),
                    cell_id: cell_id.clone(),
                    wait: wait.clone(),
                },
            );
        }
        // The Compose row is created on the cell's own `ToolStarted`, which
        // strictly precedes any nested start, so the renderer folds children
        // into an existing row and never re-parents.
        if kind == plakous_host_protocol::views::presentation::ActivityKind::Compose
            && lineage.is_composition_marker()
            && let Some(call_id) = call_id_str.as_deref()
        {
            projection.open_compose(call_id, &name, cell_id.as_deref());
        }
        if let Some(parent) = nested_parent.as_deref() {
            let reopened = projection.observe_child_start(
                parent,
                call_id_str.as_deref(),
                cell_id.as_deref(),
                lineage.nested_ordinal(),
            );
            // §5.8: a child dispatched during the `wait*` phase puts the
            // yielded cell back in flight. Re-paint the marker as running
            // and persist it so, when the turn is cancelled, the sweep over
            // persisted `running` rows reaches this compose instead of leaving
            // a `✓ Compose` header over cancelled children. The persisted
            // `started_at_ms` is kept, so the cell's accumulated duration
            // survives the yield.
            if reopened {
                let gap = projection.gap_observed;
                if let Some(compose) = projection.cells.get(parent) {
                    emit_compose_update(events, session, compose, parent, gap, None);
                }
                if let Some(mut marker) = session.transcript.tool_activity(parent) {
                    "running".clone_into(&mut marker.state);
                    marker.finished_at_ms = None;
                    observe_tool_activity(session, marker);
                }
            }
        }
        // The start stamp the row persisted above, when it has a call id.
        let started_at_ms = call_id_str.as_ref().map(|_| event_timestamp_ms(event));
        let _ = events.send(HostEvent::ToolLine {
            call_id: call_id_str,
            parent_call_id,
            cell_id,
            kind,
            name,
            state: "running".into(),
            subject,
            detail,
            duration_ms: None,
            started_at_ms,
            finished_at_ms: None,
            wait,
        });
    }

    /// A running tool call's progress.
    pub(super) fn tool_progress(self) {
        let Self {
            event,
            call_id_str,
            events,
            projection,
            ..
        } = self;
        let AgentEventKind::ToolProgress {
            identity,
            lineage,
            progress,
            ..
        } = event.kind()
        else {
            return;
        };
        // Keep row alive; never clobber subject. Optional progress detail only
        // when we have no expand body yet (TUI merges carefully). The row
        // keeps the name its start gave it (a Grok await reads as one).
        let name = presented_later_name(
            projection,
            call_id_str.as_deref(),
            identity.canonical_name().as_str().to_owned(),
            false,
        );
        let progress_detail = progress.as_ref().and_then(|p| {
            let msg = p.message().trim();
            if msg.is_empty() {
                None
            } else {
                Some(msg.chars().take(200).collect::<String>())
            }
        });
        let kind = crate::activity::tool_activity_kind(&name);
        let _ = events.send(HostEvent::ToolLine {
            call_id: call_id_str
                .or_else(|| progress.as_ref().map(|p| p.call_id().as_uuid().to_string())),
            parent_call_id: lineage_parent_call_id(lineage),
            cell_id: lineage.cell_id().map(str::to_owned),
            kind,
            name,
            state: "running".into(),
            subject: None,
            detail: progress_detail,
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        });
    }

    /// A completed tool call: its settled row, compose and child bookkeeping.
    pub(super) fn tool_completed(self) {
        let Self {
            event,
            call_id_str,
            roots,
            rows,
            store,
            home,
            session,
            events,
            delta_buf,
            last_flush,
            projection,
            log,
            observed_session_id,
            last_tree_revision,
            ..
        } = self;
        let AgentEventKind::ToolCompleted {
            identity,
            lineage,
            observation,
        } = event.kind()
        else {
            return;
        };
        flush_assistant_delta(delta_buf, last_flush, events, true);
        let name = identity.canonical_name().as_str().to_owned();
        let prior = call_id_str
            .as_ref()
            .and_then(|call_id| session.transcript.tool_activity(call_id));
        let name = presented_later_name(projection, call_id_str.as_deref(), name, true);
        // A bare pi `wait` that ran the tree's await returned its receipt
        // (a process await returns `tasks`): it reads as the await from
        // here on, as its restored row does (presentation only).
        let settled_name = plakous_host_protocol::topology::presented_settled_name(
            &name,
            None,
            observation
                .as_ref()
                .filter(|item| item.succeeded())
                .and_then(|item| serde_json::from_str::<serde_json::Value>(item.preview()).ok())
                .as_ref(),
            plakous_host_protocol::topology::is_topology_registration(
                identity.registered_tool_id().as_str(),
            ),
        );
        let bare_await = settled_name != name;
        let name = if bare_await {
            if let Some(call_id) = call_id_str.as_ref() {
                projection.process_waits.remove(call_id);
            }
            "wait_agent".to_owned()
        } else {
            name
        };
        if name == "process_session_start"
            && let Some(locator) = session.plakous_locator
            && let Some(command) = prior.as_ref().and_then(|row| row.subject.as_ref())
            && let Some(result) = observation
                .as_ref()
                .and_then(|item| serde_json::from_str::<serde_json::Value>(item.preview()).ok())
        {
            for id in crate::runtime::tools::process_ids(&result) {
                if id != "(unknown)" {
                    projection.process_commands.insert(
                        crate::runtime::tools::process_key(locator, &id),
                        command.clone(),
                    );
                }
            }
        }
        // Spec 02 §5.4: the expand body is readable text, never `Arguments:` /
        // `Result:` JSON dumps.
        let result = observation
            .as_ref()
            .map(|item| (item.succeeded(), item.preview()));
        let result_detail = completed_tool_detail(
            prior
                .as_ref()
                .and_then(|activity| activity.detail.as_deref()),
            result.map(|(_, preview)| preview),
        );
        let kind = crate::activity::tool_activity_kind(&name);
        // A call that succeeded may still report a command that failed:
        // the row names how it ended rather than painting a check (C8).
        let state = completed_tool_state(&name, result);
        log_event(
            log,
            observed_session_id,
            SessionEvent::new("tool.completed")
                .str("name", &name)
                .opt_str(
                    "exposed_name",
                    identity
                        .exposed_name()
                        .map(plakous::model::ToolName::as_str),
                )
                .opt_str("call_id", call_id_str.as_deref())
                .str("state", &state)
                .opt_str("detail", result_detail.as_deref()),
        );
        // A cell that yields before it dispatches anything never named its
        // cell on a start event — the interpreter mints the cell after the
        // marker begins — so `open_compose` learnt nothing and the `wait*`
        // continuation had no row to fold under. The marker's own terminal
        // lineage carries the retired cell, so the ownership is learnt here,
        // before anything reads it.
        if lineage.is_composition_marker()
            && let Some(call_id) = call_id_str.as_deref()
        {
            projection.learn_marker_cell(call_id, lineage.cell_id());
        }
        let nested_parent = nested_parent_call_id(lineage, projection);
        let parent_call_id = nested_parent
            .clone()
            .or_else(|| continuation_parent_call_id(&name, lineage, projection));
        let mut cell_id = lineage.cell_id().map(str::to_owned);
        // the marker owns the cell, so its own terminal line
        // carries the cell the host learnt even when the driver has already
        // retired it.
        if kind == plakous_host_protocol::views::presentation::ActivityKind::Compose
            && cell_id.is_none()
            && let Some(compose) = call_id_str
                .as_deref()
                .and_then(|call_id| projection.cells.get(call_id))
        {
            cell_id.clone_from(&compose.cell_id);
        }
        // §5.2: the completed header carries the host's own result-derived
        // counts. Only the host parses tool output for them.
        let completed_subject =
            if kind == plakous_host_protocol::views::presentation::ActivityKind::Compose {
                call_id_str
                    .as_deref()
                    .and_then(|call_id| projection.cells.get(call_id))
                    .map(|compose| compose_counts_label(compose, projection.gap_observed))
            } else {
                completed_tool_summary(
                    &name,
                    prior.as_ref().and_then(|row| row.subject.as_deref()),
                    result.map(|(_, preview)| preview),
                )
            };
        let result_succeeded = matches!(observation.as_ref(), Some(item) if item.succeeded());
        let started_wait = prior.as_ref().and_then(|row| row.wait.clone());
        let started_wait = if bare_await {
            Some(crate::runtime::tool_facts::bare_await_started(started_wait))
        } else {
            started_wait
        };
        // A topology call's whole result, as its tool left it: the
        // observation preview is bounded and stops being JSON on a
        // fan-out wait or a long conflict list.
        let call_facts = call_id_str
            .as_deref()
            .filter(|_| matches!(name.as_str(), "wait_agent" | "adopt_candidate"))
            .and_then(plakous_recursive_subagents::call_facts::take);
        let wait_result = match &call_facts {
            Some(plakous_recursive_subagents::call_facts::TopologyCallFacts::Wait(result)) => {
                Some(result.clone())
            }
            // A bare pi `wait` renamed here always leaves facts; the
            // preview is only the fallback its restored row reads too.
            _ if name == "wait_agent" && !bare_await => None,
            _ => observation
                .as_ref()
                .and_then(|item| serde_json::from_str::<serde_json::Value>(item.preview()).ok()),
        };
        let wait = wait_settled(
            &name,
            started_wait,
            wait_result.as_ref(),
            result_succeeded,
            prior
                .as_ref()
                .and_then(|row| row.started_at_ms)
                .and_then(|started| event_timestamp_ms(event).checked_sub(started)),
            call_id_str
                .as_ref()
                .and_then(|id| projection.process_waits.get(id))
                .map(Vec::as_slice),
        );
        let completed_subject = if bare_await {
            wait_result
                .as_ref()
                .and_then(crate::runtime::tool_facts::await_children)
        } else {
            completed_subject
        };
        let completed_subject = call_id_str
            .as_ref()
            .and_then(|id| projection.process_waits.remove(id))
            .map(|ids| {
                settled_process_wait_subject(
                    &ids,
                    result.map(|(_, preview)| preview),
                    session.plakous_locator,
                    &roots.job_registry().snapshot(),
                    &projection.process_commands,
                )
            })
            .or_else(|| completed_subject.map(|subject| live_tool_subject(&subject)));
        // A tool that joins a cancel reports a failed observation like any
        // other. Under a requested cancel the row is `cancelled`, so the
        // observed path and `observe_terminal_tool_activity`'s settlement
        // sweep cannot disagree about the same call.
        let row_state = if state == "succeeded" {
            "done"
        } else if projection.cancel_requested {
            "cancelled"
        } else {
            state.as_str()
        };
        // The Compose row this terminal settles: the marker's own call for an
        // `exec*`, and the cell it names for a `wait*` continuation of a
        // cell the host already opened a row for.
        let settled_compose = lineage.is_composition_marker().then(|| {
            call_id_str
                .as_deref()
                .filter(|call_id| projection.cells.contains_key(*call_id))
                .map(str::to_owned)
                .or_else(|| projection.marker_for_cell(cell_id.as_deref()))
        });
        // A marker terminal settles its Compose row's state so a child
        // dispatched during a later `wait*` cannot flip it back to running.
        // It is recorded *before* the row is painted or persisted: a cell
        // that yields while a child is still in flight has not ended, so
        // `row_state()` — not the local terminal — is what the line and the
        // transcript row carry. Otherwise a cancel in that window left
        // `✓ Compose` sitting over `⊘` children, the case `row_state` exists
        // to prevent (§5.8).
        let marker_settled_here = match settled_compose.as_ref() {
            Some(Some(marker_call)) => {
                let gap = projection.gap_observed;
                let own = call_id_str.as_deref() == Some(marker_call.as_str());
                projection.observe_compose_terminal(marker_call, row_state);
                if !own && let Some(compose) = projection.cells.get(marker_call) {
                    emit_compose_update(
                        events,
                        session,
                        compose,
                        marker_call,
                        gap,
                        observed_call_duration_ms(session, marker_call, event_timestamp_ms(event)),
                    );
                }
                own
            }
            _ => false,
        };
        let own_compose_row_state = marker_settled_here
            .then(|| {
                call_id_str
                    .as_deref()
                    .and_then(|call_id| projection.cells.get(call_id))
                    .map(|compose| compose.row_state().to_owned())
            })
            .flatten();
        let persisted_state = match own_compose_row_state.as_deref() {
            // The marker yielded with children still running: the cell is
            // not over, so the transcript keeps it `running` and the
            // re-terminalisation below writes its outcome.
            Some("running") => "running",
            // A cancelled call is recorded as cancelled, so a reopen paints
            // the same row the live surface saw and the sweep does not find
            // it still running.
            _ if projection.cancel_requested && state != "succeeded" => "cancelled",
            _ => state.as_str(),
        };
        if let Some(call_id) = call_id_str.as_ref() {
            observe_tool_activity(
                session,
                sessions::ToolItem {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    exposed_name: row_exposed_name(
                        identity.canonical_name(),
                        identity.exposed_name(),
                        &name,
                    ),
                    kind: crate::activity::tool_activity_kind(&name),
                    state: persisted_state.into(),
                    subject: completed_subject.clone().or_else(|| {
                        prior
                            .as_ref()
                            .filter(|_| !bare_await)
                            .and_then(|a| a.subject.clone())
                    }),
                    detail: result_detail
                        .clone()
                        .or_else(|| prior.as_ref().and_then(|activity| activity.detail.clone())),
                    started_at_ms: prior.as_ref().and_then(|activity| activity.started_at_ms),
                    finished_at_ms: (persisted_state != "running")
                        .then(|| event_timestamp_ms(event)),
                    parent_call_id: parent_call_id.clone(),
                    cell_id: cell_id.clone(),
                    wait: wait.clone(),
                },
            );
        }
        let compose_subject = completed_subject;
        // The row's own wall time, from the `started_at_ms` this terminal
        // just persisted alongside. A yielded cell keeps its first start,
        // so the head reports the whole cell and not the last segment
        let duration_ms = call_id_str.as_deref().and_then(|call_id| {
            observed_call_duration_ms(session, call_id, event_timestamp_ms(event))
        });
        let line = HostEvent::ToolLine {
            call_id: call_id_str.clone(),
            parent_call_id: parent_call_id.clone(),
            cell_id,
            kind,
            name,
            state: own_compose_row_state.unwrap_or_else(|| row_state.to_owned()),
            subject: compose_subject,
            detail: result_detail,
            duration_ms,
            started_at_ms: prior.as_ref().and_then(|activity| activity.started_at_ms),
            finished_at_ms: (persisted_state != "running").then(|| event_timestamp_ms(event)),
            wait,
        };
        // Inside a cancel window the cancel path publishes, so the line and
        // the `interrupted` status it precedes reach the surface in one
        // order however fast the tool reported its own terminal.
        if projection.cancel_requested {
            projection.cancel_window_terminals.push(line);
        } else {
            let _ = events.send(line);
        }
        if let Some(candidate) = call_id_str
            .as_ref()
            .and_then(|id| projection.adopt_candidates.remove(id))
            && let Some(facts) = call_facts
            && let Some(milestone) =
                crate::runtime::tool_facts::adopt_milestone(&candidate, facts, |candidate| {
                    last_tree_revision
                        .runtime(rows, session)?
                        .read(|tree| {
                            let agent = tree
                                .candidates()
                                .values()
                                .find(|record| record.candidate_id.as_str() == candidate)?
                                .producing_agent_id
                                .clone();
                            let template = tree
                                .nodes()
                                .get(&agent)
                                .and_then(|node| node.template.as_ref())
                                .map(ToString::to_string);
                            Some((agent, template))
                        })
                        .ok()
                        .flatten()
                })
        {
            let row = TranscriptRow::Milestone(milestone);
            let _ = commit_session_update(store, home, session, |candidate| {
                candidate.record_notice(row.clone());
                true
            });
            let _ = events.send(HostEvent::Row(row));
        }
        // A `wait*` terminal that closed the last child re-terminalises the
        // marker's persisted row, so the `Completed` sweep never rewrites a
        // cell that succeeded to `failed:observation-gap`.
        if let Some(Some(marker_call)) = settled_compose.as_ref()
            && !marker_settled_here
            && let Some(compose) = projection.cells.get(marker_call)
        {
            observe_settled_compose_marker(
                session,
                compose,
                projection.gap_observed,
                marker_call,
                event_timestamp_ms(event),
            );
        }
        // A child's terminal state updates its owning Compose row's tally,
        // live and in the persisted row a resume reloads.
        if let Some(parent) = nested_parent.as_deref() {
            let gap = projection.gap_observed;
            let tally = projection
                .observe_child_terminal(parent, call_id_str.as_deref(), state != "succeeded")
                .map(|compose| {
                    emit_compose_update(
                        events,
                        session,
                        compose,
                        parent,
                        gap,
                        observed_call_duration_ms(session, parent, event_timestamp_ms(event)),
                    );
                    compose_counts_label(compose, gap)
                });
            if let Some(tally) = tally {
                observe_compose_subject(session, parent, tally);
                // The last observed child of a yielded cell closing is what
                // ends the cell; write its outcome back.
                if let Some(compose) = projection.cells.get(parent) {
                    observe_settled_compose_marker(
                        session,
                        compose,
                        gap,
                        parent,
                        event_timestamp_ms(event),
                    );
                }
            }
        }
        emit_tree_snapshot_if_revision_advanced(
            rows,
            store,
            home,
            session,
            last_tree_revision,
            events,
            false,
        );
    }

    /// A tool call the runtime refused before it ran.
    pub(super) fn tool_preparation_rejected(self) {
        let Self {
            event,
            call_id_str,
            session,
            events,
            delta_buf,
            last_flush,
            tool_subjects,
            projection,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::ToolPreparationRejected {
            attempt,
            lineage,
            reason,
            detail,
            observation,
            ..
        } = event.kind()
        else {
            return;
        };
        flush_assistant_delta(delta_buf, last_flush, events, true);
        let name = attempt.model_facing_name().as_str().to_owned();
        // A rejected call never starts, so the subject stash is empty for
        // it and both identifying fields used to come out null. The event
        // now carries its own bounded redacted subject and arguments; the
        // stash stays the fallback for a call that did start.
        let info = tool_subjects.take(&name, call_id_str.as_deref());
        let rejected_subject = observation
            .as_ref()
            .and_then(ToolCallObservation::subject)
            .map(str::to_owned)
            .or_else(|| info.as_ref().map(|item| item.subject.clone()));
        let rejected_arguments = observation
            .as_ref()
            .map(|observation| observation.arguments_preview().to_owned())
            .or_else(|| info.as_ref().map(|item| item.detail.clone()));
        // The category alone does not diagnose a rejection: every schema
        // refusal is `InvalidArguments`. The descriptive message says
        // which constraint the call missed.
        let rejected_reason = tool_rejected_reason(&format!("{reason:?}"), detail.as_deref());
        let call_id = call_id_str.clone().unwrap_or_else(|| {
            format!(
                "rejected-{}-{}",
                event_timestamp_ms(event),
                session.transcript.len()
            )
        });
        observe_tool_activity(
            session,
            sessions::ToolItem {
                call_id: call_id.clone(),
                name: name.clone(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind(&name),
                state: format!("failed:rejected:{reason:?}"),
                subject: rejected_subject.clone(),
                detail: rejected_arguments.clone(),
                started_at_ms: None,
                finished_at_ms: Some(event_timestamp_ms(event)),
                parent_call_id: lineage_parent_call_id(lineage),
                cell_id: lineage.cell_id().map(str::to_owned),
                wait: None,
            },
        );
        log_event(
            log,
            observed_session_id,
            tool_rejected_record(
                &name,
                &rejected_reason,
                &call_id,
                rejected_subject.as_deref(),
                rejected_arguments.as_deref(),
            ),
        );
        let parent_call_id = lineage_parent_call_id(lineage);
        let _ = events.send(HostEvent::ToolLine {
            call_id: Some(call_id.clone()),
            parent_call_id: parent_call_id.clone(),
            cell_id: lineage.cell_id().map(str::to_owned),
            kind: crate::activity::tool_activity_kind(&name),
            name,
            state: format!("rejected:{reason:?}"),
            subject: info.as_ref().map(|i| i.subject.clone()),
            detail: info.map(|i| i.detail),
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: Some(event_timestamp_ms(event)),
            wait: None,
        });
        if let Some(parent) = parent_call_id.as_deref() {
            let gap = projection.gap_observed;
            let tally = projection
                .observe_child_terminal(parent, Some(&call_id), true)
                .map(|compose| {
                    emit_compose_update(
                        events,
                        session,
                        compose,
                        parent,
                        gap,
                        observed_call_duration_ms(session, parent, event_timestamp_ms(event)),
                    );
                    compose_counts_label(compose, gap)
                });
            if let Some(tally) = tally {
                observe_compose_subject(session, parent, tally);
            }
        }
    }
}

/// The row name a tool start shows: a Grok output or kill call naming a child
/// reads as the await or the interrupt it is (presentation only), and the
/// call's later lines keep that name.
/// The exposed name a live row carries: the name the model called, when it
/// differs from the canonical row name and presentation did not rename the
/// call (a renamed call keeps no exposed name, as its restored row does).
fn row_exposed_name(
    canonical: &plakous::model::ToolName,
    exposed: Option<&plakous::model::ToolName>,
    name: &str,
) -> Option<String> {
    exposed
        .map(plakous::model::ToolName::as_str)
        .filter(|exposed| *exposed != name && name == canonical.as_str())
        .map(str::to_owned)
}

pub(in crate::runtime) fn presented_start_name<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    call_id: Option<&str>,
    canonical: &str,
    arguments: Option<&serde_json::Value>,
    topology_call: bool,
) -> String {
    let name =
        plakous_host_protocol::topology::presented_slot_name(canonical, arguments, topology_call);
    if name != canonical
        && let Some(call_id) = call_id
    {
        projection
            .presented_names
            .insert(call_id.to_owned(), name.to_owned());
    }
    name.to_owned()
}

/// The row name a progress or terminal line of a call shows: the one its
/// start gave it. A terminal line forgets it.
pub(in crate::runtime) fn presented_later_name<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    call_id: Option<&str>,
    canonical: String,
    terminal: bool,
) -> String {
    let Some(call_id) = call_id else {
        return canonical;
    };
    let presented = if terminal {
        projection.presented_names.remove(call_id)
    } else {
        projection.presented_names.get(call_id).cloned()
    };
    presented.unwrap_or(canonical)
}

#[cfg(test)]
mod tests;
