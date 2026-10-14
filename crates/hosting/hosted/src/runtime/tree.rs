//! Topology tree bridging: lineage, snapshots, and sync dispatch.

use crate::session_log::SessionEvent;
use plakous::application::ApplicationExecutor;

use crate::sessions::SessionRecord;

use super::rebind::durable_tree_id;
use super::{HostEvent, Sender, TopologyRows, TreeSnapshotPush, TurnProjection, UserAction};
use crate::sessions::{NoticeScope, TranscriptRow};
use plakous_host_protocol::views::transcript::{AgentMilestone, MilestoneEvent};

/// One bounded subject for unread child mail: id, child, and status — not the payload.
fn child_result_available_subject(message_id: &str, body: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let status = parsed
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(serde_json::Value::as_str);
    let from = parsed
        .as_ref()
        .and_then(|value| value.get("from"))
        .and_then(serde_json::Value::as_str);
    match (from, status) {
        (Some(from), Some(status)) => format!("{message_id} · {from} {status}"),
        (None, Some(status)) => format!("{message_id} · {status}"),
        (Some(from), None) => format!("{message_id} · {from}"),
        (None, None) => message_id.to_owned(),
    }
}

/// Projects the owning local-composition marker call from event lineage.
///
/// `None` for a direct call and for the outer marker itself. Carried on
/// `ToolLine` so the presentation layer has a grouping key it never has to
/// parse out of a display string.
pub(super) fn lineage_parent_call_id(
    lineage: &plakous::agent::ToolCallerLineageView,
) -> Option<String> {
    lineage
        .parent_call_id()
        .map(|call_id| call_id.as_uuid().to_string())
}

/// The Compose row a nested call belongs to.
///
/// A nested call normally names its parent directly. When it names only the
/// cell it ran inside, the owning marker is still an *observed* fact — the host
/// learnt it when it opened that cell's row — so the child is tallied there
/// rather than dropped. This is not re-parenting or guesswork: a cell id the
/// host never opened still yields `None` and the child stays a top-level row.
///
/// Mini-session (2026-09-03): cells that did real work rendered
/// `✓ Compose 0 tools` because every nested call arrived cell-identified but
/// parent-less.
pub(super) fn nested_parent_call_id<P: plakous_host_protocol::ProtocolExtension>(
    lineage: &plakous::agent::ToolCallerLineageView,
    projection: &TurnProjection<P>,
) -> Option<String> {
    if let Some(parent) = lineage_parent_call_id(lineage) {
        return Some(parent);
    }
    if lineage.is_composition_marker() || lineage.nested_ordinal().is_none() {
        return None;
    }
    projection.marker_for_cell(lineage.cell_id())
}

/// The Compose row a `wait*` continuation folds under (§5.8).
///
/// The continuation names the cell it resumes and no parent;
/// the host learnt that cell's marker when the cell opened, so the row is
/// projected under it in transcript order — live, the row otherwise sat flat
/// between children three and four and split the rail. It is a presentation
/// parent only: the continuation is never tallied as a child. A cell the host
/// never opened stays a flat row.
pub(super) fn continuation_parent_call_id<P: plakous_host_protocol::ProtocolExtension>(
    name: &str,
    lineage: &plakous::agent::ToolCallerLineageView,
    projection: &TurnProjection<P>,
) -> Option<String> {
    if !crate::activity::is_local_composition_continuation(name) || !lineage.is_composition_marker()
    {
        return None;
    }
    projection.marker_for_cell(lineage.cell_id())
}

impl TreeSnapshotPush {
    /// The topology runtime this session's canvas is watching, if any.
    ///
    /// Resolution is at most one lineage walk per bound session. Both recovery
    /// paths below are hub-map lookups, never a store open, so the streamed-event
    /// cost stays a single `HashMap` probe.
    pub(super) fn runtime(
        &mut self,
        rows: &dyn TopologyRows,
        session: &SessionRecord,
    ) -> Option<std::sync::Arc<crate::topology_runtime::ProductTopologyRuntime>> {
        if self.session_id.as_deref() != Some(session.id.as_str()) {
            self.child_live = super::child_live::ChildObservers::default();
            self.session_id = Some(session.id.clone());
            self.tree_id = None;
            self.last_revision = None;
            self.notified_results.clear();
            self.child_lifecycles = None;
            self.announced_questions.clear();
        }
        let cached = self
            .tree_id
            .get_or_insert_with(|| durable_tree_id(rows, session));
        if let Some(runtime) = crate::topology_runtime::lookup(cached) {
            return Some(runtime);
        }
        // A tree minted for this session *after* the lineage walk resolved an
        // ancestor's id: adopt it rather than pushing nothing for the attach.
        let runtime = crate::topology_runtime::lookup(&session.id)?;
        session.id.clone_into(cached);
        self.last_revision = None;
        Some(runtime)
    }

    /// The durable tree id [`Self::runtime`] last resolved, if any.
    pub(super) fn tree_id(&self) -> Option<&str> {
        self.tree_id.as_deref()
    }
}

fn snapshot_due(
    last_revision: Option<u64>,
    revision: u64,
    changed: bool,
    next: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    last_revision != Some(revision) || (changed && next.is_none_or(|next| now >= next))
}

/// What a milestone compares between two tree reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ChildLifecycle {
    status: plakous_host_protocol::topology::NodeStatus,
    attempt: Option<String>,
    running_since: Option<std::time::Instant>,
}

/// Text a failed or needs-recovery milestone keeps: the row shows its first
/// line and the detail the rest, so it is bounded like a tool row's detail.
const MILESTONE_TEXT_BYTES: usize = crate::sessions::MAX_TOOL_ACTIVITY_DETAIL_BYTES;

/// A finished child's submitted result: its row opens to nothing else, so it
/// keeps more than a reason does.
const RESULT_TEXT_BYTES: usize = 16 * 1024;

fn milestone_text(text: &str) -> Option<String> {
    bounded_text(text, MILESTONE_TEXT_BYTES)
}

/// A submitted result as the child sent it (the renderer indents JSON).
fn result_text(output_json: &str) -> Option<String> {
    bounded_text(output_json, RESULT_TEXT_BYTES)
}

/// `text` trimmed, or its head within `bound` bytes and a last line that
/// says it was cut and from how much.
fn bounded_text(text: &str, bound: usize) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.len() <= bound {
        return Some(text.to_owned());
    }
    let cut = format!(
        "\n… cut at {} KiB of {} KiB",
        bound / 1024,
        text.len().div_ceil(1024)
    );
    let mut end = bound.saturating_sub(cut.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    // Multi-line text is cut at a line end, so no line is half shown.
    if let Some(line_end) = text[..end].rfind('\n') {
        end = line_end;
    }
    Some(format!("{}{cut}", text[..end].trim_end()))
}

/// The milestone rows one child's transition earns, oldest first. A finished
/// child carries its submitted result, a failed one its failure message, and
/// one that needs recovery the tree's fault reason (or its failure message).
fn child_milestones(
    tree: &plakous_recursive_subagents::tree::ProductAgentTree,
    node: &plakous_recursive_subagents::tree::NodeRecord,
    previous: Option<&ChildLifecycle>,
    now: std::time::Instant,
) -> (ChildLifecycle, Vec<AgentMilestone>) {
    use plakous_host_protocol::topology::NodeStatus;

    let attempt = node.attempt_id.as_ref().map(ToString::to_string);
    let running = matches!(node.status, NodeStatus::Running);
    let running_since = match previous {
        Some(previous) if previous.attempt == attempt => previous.running_since,
        _ => None,
    }
    .or_else(|| running.then_some(now));
    let current = ChildLifecycle {
        status: node.status,
        attempt,
        running_since,
    };
    let milestone = |event, text: Option<String>| AgentMilestone {
        agent_id: node.agent_id.clone(),
        event,
        template: node.template.as_ref().map(ToString::to_string),
        text,
        duration_ms: None,
        candidate: None,
        files: None,
        paths: Vec::new(),
    };
    let brief = Some(node.description.clone()).filter(|text| !text.is_empty());
    let mut rows = Vec::new();
    match previous {
        None => rows.push(milestone(MilestoneEvent::Spawned, brief)),
        Some(previous) if previous.attempt.is_some() && previous.attempt != current.attempt => {
            rows.push(milestone(MilestoneEvent::Retasked, brief));
        }
        Some(_) => {}
    }
    if previous.is_some_and(|previous| previous.status == current.status) {
        return (current, rows);
    }
    let attempt = tree.attempt(&node.agent_id);
    let failure = attempt
        .and_then(|attempt| attempt.failure.as_ref())
        .map(|failure| failure.message.as_str());
    match node.status {
        NodeStatus::Completed => rows.push(AgentMilestone {
            duration_ms: running_since.map(|since| {
                u64::try_from(now.duration_since(since).as_millis()).unwrap_or(u64::MAX)
            }),
            template: None,
            ..milestone(
                MilestoneEvent::Finished,
                attempt
                    .and_then(|attempt| attempt.output_json.as_deref())
                    .and_then(result_text),
            )
        }),
        NodeStatus::Failed => rows.push(AgentMilestone {
            template: None,
            ..milestone(MilestoneEvent::Failed, failure.and_then(milestone_text))
        }),
        NodeStatus::RecoveryRequired => rows.push(AgentMilestone {
            template: None,
            ..milestone(
                MilestoneEvent::NeedsRecovery,
                tree.fault_reason(&node.agent_id)
                    .or(failure)
                    .and_then(milestone_text),
            )
        }),
        _ => {}
    }
    (current, rows)
}

/// Questions children addressed to the root, in mailbox order.
fn root_questions(
    tree: &plakous_recursive_subagents::tree::ProductAgentTree,
) -> impl Iterator<Item = &plakous_recursive_subagents::tree::MailRecord> {
    tree.messages().values().filter(|mail| {
        mail.to == tree.root_agent_id
            && mail.purpose == plakous_host_protocol::topology::MessagePurpose::Question
    })
}

/// `? c23 asks · <question>`.
fn ask_milestone(
    tree: &plakous_recursive_subagents::tree::ProductAgentTree,
    question: &plakous_recursive_subagents::tree::MailRecord,
) -> AgentMilestone {
    AgentMilestone {
        agent_id: question.from.clone(),
        event: MilestoneEvent::Asks,
        template: tree
            .nodes()
            .get(&question.from)
            .and_then(|node| node.template.as_ref())
            .map(ToString::to_string),
        text: Some(question.body.clone()).filter(|text| !text.is_empty()),
        duration_ms: None,
        candidate: None,
        files: None,
        paths: Vec::new(),
    }
}

/// Publish on durable revisions or material child progress changes. Progress-only
/// snapshots are limited to one per second; timestamps alone never project rows.
pub(super) fn emit_tree_snapshot_if_revision_advanced<
    P: plakous_host_protocol::ProtocolExtension,
>(
    rows: &dyn TopologyRows,
    store: &dyn crate::session_store::SessionRecordStore,
    home: &crate::session_home::SessionHome,
    session: &mut SessionRecord,
    push: &mut TreeSnapshotPush,
    events: &Sender<P>,
    skip_if_first: bool,
) {
    let Some(runtime) = push.runtime(rows, session) else {
        return;
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        push.child_live.sync(&runtime);
    }
    let child_live = &mut push.child_live;
    let last_revision = &mut push.last_revision;
    let next_progress_snapshot = &mut push.next_progress_snapshot;
    let notified_results = &mut push.notified_results;
    let announced_abandoned = &mut push.announced_abandoned_turns;
    let child_lifecycles = &mut push.child_lifecycles;
    let announced_questions = &mut push.announced_questions;
    let milestones = runtime.read(|tree| {
        let now = std::time::Instant::now();
        let seeded = child_lifecycles.is_some();
        let known = child_lifecycles.get_or_insert_with(Default::default);
        let mut rows = Vec::new();
        for node in tree.nodes().values() {
            if node.agent_id == tree.root_agent_id {
                continue;
            }
            let (current, earned) = child_milestones(tree, node, known.get(&node.agent_id), now);
            if seeded {
                rows.extend(earned);
            }
            known.insert(node.agent_id.clone(), current);
        }
        for question in root_questions(tree) {
            if announced_questions.insert(question.message_id.clone()) && seeded {
                rows.push(ask_milestone(tree, question));
            }
        }
        rows
    });
    // No session feed fact implies a tree milestone, so each is an operator
    // notice intent (spec 02 §2.6) and a restore shows what the live face did.
    for milestone in milestones.unwrap_or_default() {
        let row = TranscriptRow::Milestone(milestone);
        let _ = super::admission::commit_session_update(store, home, session, |candidate| {
            candidate.record_notice(row.clone());
            true
        });
        let _ = events.send(HostEvent::Row(row));
    }
    let _ = runtime.read(|tree| {
        let revision = tree.revision;
        let now = std::time::Instant::now();
        if !snapshot_due(
            *last_revision,
            revision,
            child_live.progress_changed(),
            *next_progress_snapshot,
            now,
        ) {
            return;
        }
        let mut snapshot = crate::topology_navigator::project_snapshot(tree);
        child_live.project(&mut snapshot);
        *next_progress_snapshot = Some(now + std::time::Duration::from_secs(1));
        let first = last_revision.is_none();
        *last_revision = Some(revision);
        let pending = tree.pending_result_mail(&tree.root_agent_id);
        notified_results.retain(|id| pending.iter().any(|mail| &mail.message_id == id));
        for mail in pending {
            if notified_results.contains(&mail.message_id) {
                continue;
            }
            let call_id = format!("child-result:{}", mail.message_id);
            let subject = crate::sessions::bound_subject(&child_result_available_subject(
                &mail.message_id,
                &mail.body,
            ));
            // A tree row is a product notice intent (spec 02 §2.6), recorded
            // once per mail: a restored transcript already holds it.
            let row = crate::sessions::ToolItem {
                call_id: call_id.clone(),
                name: "child_result".into(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind("child_result"),
                state: "done".into(),
                subject: Some(subject.clone()),
                detail: None,
                started_at_ms: None,
                finished_at_ms: None,
                parent_call_id: None,
                cell_id: None,
                wait: None,
            };
            if session.transcript.tool_activity(&call_id).is_none() {
                let _ =
                    super::admission::commit_session_update(store, home, session, |candidate| {
                        candidate.record_notice(TranscriptRow::Tool(row));
                        true
                    });
            }
            if events.send(HostEvent::ToolLine {
                call_id: Some(call_id),
                parent_call_id: None,
                cell_id: None,
                kind: plakous_host_protocol::views::presentation::ActivityKind::Agent,
                name: "child_result".into(),
                state: "done".into(),
                subject: Some(subject),
                detail: None,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            }) {
                notified_results.insert(mail.message_id);
            }
        }
        // A child session is reopened inside the caller's tool work, where no
        // face is listening, so a reopen that abandoned an unfinished model
        // request left the fact on the node. This is the caller's face reading
        // it: once per node, because it describes one reopen rather than a
        // standing condition, and into the log as well as the transcript so it
        // outlives the process that printed it.
        for node in tree.nodes().values() {
            let Some(abandoned) = node.abandoned_turn.as_ref() else {
                continue;
            };
            if announced_abandoned.contains(&node.agent_id) {
                continue;
            }
            if events.notice(TranscriptRow::info(
                NoticeScope::Tree,
                abandoned.note(&node.agent_id),
            )) {
                let mut record = SessionEvent::new("turn.abandoned")
                    .str("id", &session.id)
                    .str("agent_id", &node.agent_id)
                    .u64("operations", abandoned.operations.len() as u64);
                for (index, operation) in abandoned.operations.iter().enumerate() {
                    record = record.u64(&format!("operation_{index}"), *operation);
                }
                crate::session_log::emit(home, &session.id, record);
                announced_abandoned.insert(node.agent_id.clone());
            }
        }
        if first && skip_if_first {
            return;
        }
        crate::topology_navigator::apply_session_root_binding(&mut snapshot, session);
        let _ = events.send(HostEvent::TopologyNavigatorSnapshot(snapshot));
    });
}

pub(super) fn emit_child_progress_on_refresh<P: plakous_host_protocol::ProtocolExtension>(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    session: &SessionRecord,
    push: &mut TreeSnapshotPush,
    events: &Sender<P>,
    action: &UserAction<P>,
) {
    if !matches!(
        action,
        UserAction::OpenTopologyNavigator | UserAction::RefreshTopologyTree
    ) {
        return;
    }
    push.child_live.sync(runtime);
    if let Ok(mut snapshot) = runtime.read(crate::topology_navigator::project_snapshot) {
        push.child_live.project(&mut snapshot);
        crate::topology_navigator::apply_session_root_binding(&mut snapshot, session);
        snapshot.solicited = true;
        if snapshot
            .nodes
            .iter()
            .any(|node| node.live_progress.is_some())
        {
            let _ = events.send(HostEvent::TopologyNavigatorSnapshot(snapshot));
        }
    }
}

pub(super) fn record_emitted_tree_revision(
    rows: &dyn TopologyRows,
    session: &SessionRecord,
    push: &mut TreeSnapshotPush,
) {
    let Some(runtime) = push.runtime(rows, session) else {
        return;
    };
    if let Ok(revision) = runtime.read(|tree| tree.revision) {
        push.last_revision = Some(revision);
    }
}

/// Starts `tree_id`'s background child-recovery pass. Must run on the host's
/// executor. Its report reaches the user as one system note when it
/// finishes, so the host loop never waits on a pass.
///
/// `explicit` is the operator's `/recover`: it also says that a pass started
/// (when it has children to try), that a running pass will run once more, or
/// that the tree is paused, and `nothing` is the
/// note for a pass that found nothing to do.
pub(super) fn start_child_recovery<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session_id: &str,
    tree_id: String,
    explicit: bool,
    nothing: Option<String>,
) -> crate::agent::RecoveryPassStart {
    // The pass reports after the host may have switched sessions.
    let finished = events.pinned_to(session_id);
    // The automatic pass runs on every open and reports the children it still
    // could not settle each time, so its report is live only; the operator's
    // own `/recover` answer is a one-time event and is stored.
    let report_row = move |row: TranscriptRow| {
        if explicit {
            finished.notice(row)
        } else {
            finished.send(HostEvent::Row(row))
        }
    };
    let started = crate::agent::start_tree_recovery_pass(tree_id, move |report| {
        match report.and_then(|report| report.note()) {
            Some(note) => {
                let _ = report_row(TranscriptRow::info(NoticeScope::Tree, note));
            }
            None => {
                if let Some(nothing) = nothing {
                    let _ = report_row(TranscriptRow::info(NoticeScope::Tree, nothing));
                }
            }
        }
    });
    if explicit {
        let note = match started {
            crate::agent::RecoveryPassStart::Started { pending } if pending > 0 => Some(format!(
                "plak: recovering {pending} child agent(s) in the background; a note follows \
                 when the pass finishes"
            )),
            crate::agent::RecoveryPassStart::RerunQueued => Some(
                "plak: a child recovery pass is running; it runs once more and reports when it \
                 finishes"
                    .to_owned(),
            ),
            crate::agent::RecoveryPassStart::Paused => Some(
                "plak: this agent tree is paused; resuming it (r on /tree, or --resume-tree) \
                 recovers its children"
                    .to_owned(),
            ),
            _ => None,
        };
        if let Some(note) = note {
            let _ = events.notice(TranscriptRow::info(NoticeScope::Tree, note));
        }
    }
    started
}

pub(super) fn dispatch_topology_navigator_sync<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    rows: &dyn TopologyRows,
    roots: &crate::agent::ProductStateRoots,
    session: &SessionRecord,
    events: &Sender<P>,
    action: &UserAction<P>,
    push: &mut TreeSnapshotPush,
) {
    let resumed = matches!(action, UserAction::ResumeTopologyTree);
    if let Err(error) = executor.block_on(async {
        crate::topology_navigator::dispatch_topology_navigator_action(
            rows,
            session,
            |event| {
                let _ = events.send(event);
            },
            action,
        )
        .await;
        if let Some(runtime) = push.runtime(rows, session) {
            push.child_live
                .set_jobs(std::sync::Arc::clone(roots.job_registry()));
            push.child_live
                .action(
                    &runtime,
                    roots.runtime_sessions().ok(),
                    &session.id,
                    action,
                    events,
                )
                .await;
            emit_child_progress_on_refresh(&runtime, session, push, events, action);
        }
        if resumed {
            start_child_recovery(
                events,
                &session.id,
                durable_tree_id(rows, session),
                false,
                None,
            );
        }
    }) {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Tree,
            format!("agent tree navigator could not run: {error}"),
        ));
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one idle tool dispatch names its whole exact context"
)]
pub(super) fn dispatch_invoke_tool_sync<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    rows: &dyn TopologyRows,
    store: &dyn crate::session_store::SessionRecordStore,
    home: &crate::session_home::SessionHome,
    session: &mut SessionRecord,
    events: &Sender<P>,
    name: &str,
    arguments: &str,
    agent_id: Option<&str>,
    correlation_id: Option<&str>,
    last_tree_revision: &mut TreeSnapshotPush,
) {
    let value = match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) if value.is_object() => value,
        _ => {
            let _ = events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                "invoke_tool arguments must be a JSON object",
            ));
            return;
        }
    };
    let tree_id = durable_tree_id(rows, session);
    let call_id = uuid::Uuid::new_v4().to_string();
    match executor.block_on(crate::agent::invoke_topology_tool_on_hub(
        &tree_id, agent_id, name, &value, &call_id,
    )) {
        Ok(Ok(receipt)) => {
            if let Some(command_id) = correlation_id {
                let _ = events.send(HostEvent::ToolInvocationOrigin {
                    call_id: receipt.call_id.clone(),
                    command_id: command_id.to_owned(),
                });
            }
            let kind = crate::activity::tool_activity_kind(&receipt.name);
            let _ = events.send(HostEvent::ToolLine {
                call_id: Some(receipt.call_id.clone()),
                parent_call_id: None,
                cell_id: None,
                kind,
                name: receipt.name.clone(),
                state: "start".into(),
                subject: None,
                detail: None,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            });
            let state = if receipt.succeeded { "done" } else { "failed" };
            let detail = (!receipt.detail.is_empty()).then_some(receipt.detail);
            let _ = events.send(HostEvent::ToolLine {
                call_id: Some(receipt.call_id),
                parent_call_id: None,
                cell_id: None,
                kind,
                name: receipt.name.clone(),
                state: state.into(),
                subject: None,
                detail,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            });
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
        Ok(Err(message)) => {
            let _ = events.notice(TranscriptRow::error(NoticeScope::Tree, message));
        }
        Err(error) => {
            let _ = events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                format!("invoke_tool could not run: {error}"),
            ));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;

    use super::*;
    use crate::runtime::*;

    use crate::runtime::test_kit::MemoryRows;
    use crate::runtime::test_support::*;
    use crate::sessions::SessionRecord;

    /// A finished child's result keeps 16 KiB and a cut says so and from how
    /// much, instead of ending mid-token behind `…`; multi-line text is cut
    /// at a line end.
    #[test]
    fn a_cut_milestone_text_says_it_was_cut() {
        assert_eq!(
            result_text(r#" {"verdict":"ok"} "#).as_deref(),
            Some(r#"{"verdict":"ok"}"#)
        );
        assert_eq!(result_text(" "), None);

        let long = format!(r#"{{"notes":[{}]}}"#, vec![r#""ab""#; 8_000].join(","));
        let text = result_text(&long).unwrap();
        assert!(text.len() <= RESULT_TEXT_BYTES, "{}", text.len());
        let (head, last) = text.split_once('\n').unwrap();
        assert!(head.starts_with(r#"{"notes":["ab","#), "{head}");
        assert_eq!(last, "… cut at 16 KiB of 40 KiB");

        let lines = "a reason line\n".repeat(400);
        let text = milestone_text(&lines).unwrap();
        assert!(text.len() <= MILESTONE_TEXT_BYTES);
        let (head, last) = text.rsplit_once('\n').unwrap();
        assert!(head.ends_with("a reason line"), "{head:?}");
        assert_eq!(last, "… cut at 2 KiB of 6 KiB");

        let reason = "é".repeat(MILESTONE_TEXT_BYTES);
        let text = milestone_text(&reason).unwrap();
        assert!(text.len() <= MILESTONE_TEXT_BYTES);
        assert!(text.ends_with("é\n… cut at 2 KiB of 4 KiB"), "{text}");
    }

    /// Host `ToolLine` carries the owning marker call for a nested tool.
    ///
    /// The grouping key the renderer needs is projected from the
    /// event's caller lineage, never parsed out of a display string. A direct
    /// call and the outer marker itself carry no parent.
    #[test]
    fn tool_line_parent_call_id_projects_event_caller_lineage() {
        use plakous::agent::{ToolCallerClass, ToolCallerLineageView};
        use plakous::model::{CallId, ToolCallerLineage};

        let parent = CallId::try_from(uuid::Uuid::from_u128(0x0104)).unwrap();
        let child = CallId::try_from(uuid::Uuid::from_u128(0x0105)).unwrap();

        let direct = ToolCallerLineageView::direct();
        assert_eq!(direct.class(), ToolCallerClass::Direct);
        assert_eq!(lineage_parent_call_id(&direct), None);

        let marker = ToolCallerLineageView::local_composition_marker(Some("cell-1"));
        assert_eq!(marker.class(), ToolCallerClass::LocalProgrammatic);
        assert!(marker.is_composition_marker());
        assert_eq!(lineage_parent_call_id(&marker), None);

        let canonical = ToolCallerLineage::local_programmatic(7, 1, "cell-1", 2, child, 1).unwrap();
        let nested = ToolCallerLineageView::from_lineage(&canonical, Some(parent));
        assert_eq!(nested.class(), ToolCallerClass::LocalProgrammatic);
        assert_eq!(nested.cell_id(), Some("cell-1"));
        assert_eq!(nested.nested_ordinal(), Some(2));
        assert_eq!(nested.depth(), Some(1));
        assert!(!nested.is_composition_marker());
        assert_eq!(
            lineage_parent_call_id(&nested),
            Some(parent.as_uuid().to_string())
        );

        // The projection is exactly what the emitted line carries.
        let line = HostEvent::ToolLine {
            call_id: Some(child.as_uuid().to_string()),
            parent_call_id: lineage_parent_call_id(&nested),
            cell_id: nested.cell_id().map(str::to_owned),
            kind: plakous_host_protocol::views::presentation::ActivityKind::Tool,
            name: "read_file".into(),
            state: "running".into(),
            subject: None,
            detail: None,
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        };
        let HostEvent::ToolLine {
            parent_call_id: emitted,
            ..
        } = line
        else {
            unreachable!("constructed a ToolLine")
        };
        assert_eq!(emitted, Some(parent.as_uuid().to_string()));
    }

    #[tokio::test]
    async fn tree_snapshot_pushes_on_any_node_revision_and_drops_non_advancing() {
        use crate::agent_tree::ProductAgentTree;
        use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

        let root = tempfile::tempdir().unwrap();
        let paths = MemoryRows::new();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let tree_id = durable_tree_id(&paths, &session);
        let tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.clone(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
        let (sender, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(sender);
        let mut last_revision = TreeSnapshotPush::default();
        let home = SessionHome::new(std::env::temp_dir().join("plak-tree-test-sessions"));

        // The first observation only records the revision: attach already drew.
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut last_revision,
            &events,
            true,
        );
        assert!(received.try_recv().is_err());

        // A child spawn is a node revision with no root tool receipt behind it.
        {
            let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), Some("op-spawn"))
                .unwrap();
        }
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut last_revision,
            &events,
            false,
        );
        // The spawn is a persisted milestone row first, then the canvas push.
        let HostEvent::Row(TranscriptRow::Milestone(spawned)) = received.try_recv().unwrap() else {
            panic!("child spawn must publish its milestone row");
        };
        assert_eq!(spawned.event, MilestoneEvent::Spawned);
        assert!(
            session
                .transcript
                .contains(&TranscriptRow::Milestone(spawned))
        );
        let HostEvent::TopologyNavigatorSnapshot(snapshot) = received.try_recv().unwrap() else {
            panic!("child spawn must reach the canvas");
        };
        assert_eq!(snapshot.nodes.len(), 2);
        assert!(!snapshot.solicited);

        // The same revision is never sent twice.
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut last_revision,
            &events,
            false,
        );
        assert!(received.try_recv().is_err(), "non-advancing revision drops");

        // A status transition on the child is another revision, and it pushes.
        let child = snapshot.nodes[1].agent_id.clone();
        {
            let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.set_active_turn(&child, true).unwrap();
        }
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut last_revision,
            &events,
            false,
        );
        let HostEvent::TopologyNavigatorSnapshot(snapshot) = received.try_recv().unwrap() else {
            panic!("a child status transition must reach the canvas");
        };
        assert!(snapshot.nodes[1].active_turn);

        // A question the child addresses to the root is an `asks` milestone,
        // persisted like the others and said once.
        {
            let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.send_agent_message(
                &child,
                &crate::agent_tree_protocol::SendAgentMessageRequest {
                    agent_id: "root".into(),
                    message: "which branch?\nmain or a2".into(),
                    purpose: Some(crate::agent_tree_protocol::MessagePurpose::Question),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        }
        for _ in 0..2 {
            emit_tree_snapshot_if_revision_advanced(
                &paths,
                paths.records(),
                &home,
                &mut session,
                &mut last_revision,
                &events,
                false,
            );
        }
        let asks: Vec<_> = received
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Milestone(milestone)) => Some(milestone),
                _ => None,
            })
            .collect();
        let [asked] = asks.as_slice() else {
            panic!("one asks milestone expected: {asks:?}");
        };
        assert_eq!(asked.event, MilestoneEvent::Asks);
        assert_eq!(asked.agent_id, child);
        assert_eq!(asked.text.as_deref(), Some("which branch?\nmain or a2"));
        assert!(
            session
                .transcript
                .contains(&TranscriptRow::Milestone(asked.clone()))
        );

        // A finished child carries its submitted result, a failed one its
        // failure message, live and persisted alike.
        let failing = {
            let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.submit_result(
                &child,
                &crate::agent_tree_protocol::SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(&child).unwrap();
            tree.spawn_agent(
                "root",
                &SpawnAgentRequest::test_review(),
                Some("op-spawn-2"),
            )
            .unwrap()
            .agent_id
        };
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut last_revision,
            &events,
            false,
        );
        {
            let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.record_turn_failure(
                &failing,
                crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                    kind: "provider_error",
                },
                "the provider refused the request\nretry later".into(),
            )
            .unwrap();
        }
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut last_revision,
            &events,
            false,
        );
        let terminal: Vec<_> = received
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Milestone(milestone))
                    if matches!(
                        milestone.event,
                        MilestoneEvent::Finished | MilestoneEvent::Failed
                    ) =>
                {
                    Some(milestone)
                }
                _ => None,
            })
            .collect();
        let [finished, failed] = terminal.as_slice() else {
            panic!("a finished and a failed milestone expected: {terminal:?}");
        };
        assert_eq!(finished.agent_id, child);
        assert_eq!(finished.text.as_deref(), Some(r#"{"ok":true}"#));
        assert_eq!(failed.agent_id, failing);
        assert_eq!(
            failed.text.as_deref(),
            Some("the provider refused the request\nretry later")
        );
        for row in [finished, failed] {
            assert!(
                session
                    .transcript
                    .contains(&TranscriptRow::Milestone(row.clone()))
            );
        }
        crate::topology_runtime::force_release(&tree_id).await;
    }

    /// The durable tree id is resolved once per bound session, never per event.
    ///
    /// `durable_topology_tree_id` walks the session's rebind lineage and opens
    /// the product store on a hub miss, so calling it after every streamed
    /// event pays a sqlite open per `TextDelta`.
    #[tokio::test]
    async fn the_tree_push_resolves_its_tree_id_once_per_bound_session() {
        let root = tempfile::tempdir().unwrap();
        let paths = MemoryRows::new();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let tree_id = durable_tree_id(&paths, &session);
        crate::topology_runtime::insert_root(
            tree_id.clone(),
            crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
                &tree_id,
                "root",
                "owner",
                root.path().to_path_buf(),
                "selected-tuple",
                crate::agent_tree_protocol::WriterMode::SingleLease,
            )
            .unwrap(),
            None,
        )
        .unwrap();
        let mut push = TreeSnapshotPush::default();
        assert!(push.runtime(&paths, &session).is_some());
        assert_eq!(push.tree_id.as_deref(), Some(tree_id.as_str()));

        // A later lineage change on the same session cannot re-resolve it:
        // one bound session owns one tree id.
        session.source_product_session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(push.runtime(&paths, &session).is_some());
        assert_eq!(push.tree_id.as_deref(), Some(tree_id.as_str()));

        // Once the tree leaves the hub the emitter
        // still answers from two hub-map probes and never re-walks the lineage,
        // so a streamed event costs no store open.
        crate::topology_runtime::force_release(&tree_id).await;
        assert!(push.runtime(&paths, &session).is_none());
        assert_eq!(push.tree_id.as_deref(), Some(tree_id.as_str()));
    }

    /// A child's abandoned turn reaches the caller's face, once.
    ///
    /// A child session is reopened inside the caller's tool work, where no face
    /// is listening, so the reopen leaves the fact on the node. This is the
    /// caller reading it. The line names the node, because a caller may be
    /// running several, and it says the request may have been billed, which is
    /// the operator's only evidence that a provider call may have been charged
    /// for output nobody kept. Saying it on every snapshot would make a
    /// one-off read as a standing condition, so it is said once.
    #[tokio::test]
    async fn a_childs_abandoned_turn_reaches_the_callers_face_once() {
        use crate::agent_tree::ProductAgentTree;
        use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

        let root = tempfile::tempdir().unwrap();
        let paths = MemoryRows::new();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let tree_id = durable_tree_id(&paths, &session);
        let tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.clone(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
        let (sender, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(sender);
        let mut push = TreeSnapshotPush::default();
        let home = SessionHome::new(root.path().join("sessions"));
        {
            let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            let spawned = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), Some("op-spawn"))
                .unwrap();
            tree.record_abandoned_turn(
                &spawned.agent_id,
                crate::agent_tree::AbandonedChildTurn {
                    operations: vec![7],
                    claim: plakous::agent::AbandonedTurnClaim::ModelRequest,
                },
            )
            .unwrap();
        }
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut push,
            &events,
            false,
        );
        let mut notes = Vec::new();
        while let Ok(event) = received.try_recv() {
            if let HostEvent::Row(TranscriptRow::Notice(
                plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
            )) = event
            {
                notes.push(note);
            }
        }
        let abandoned: Vec<&String> = notes
            .iter()
            .filter(|note| note.contains("recovered an interrupted turn"))
            .collect();
        assert_eq!(abandoned.len(), 1, "{notes:?}");
        let note = abandoned[0];
        assert!(note.contains("c1"), "the line names the node: {note}");
        assert!(note.contains("processed and billed"), "{note}");
        assert!(
            note.contains("no response was kept in the conversation"),
            "{note}"
        );

        // A later snapshot of the same tree does not say it again.
        {
            let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.spawn_agent(
                "root",
                &SpawnAgentRequest::test_review(),
                Some("op-spawn-2"),
            )
            .unwrap();
        }
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut push,
            &events,
            false,
        );
        while let Ok(event) = received.try_recv() {
            assert!(
                !matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("recovered an interrupted turn")),
                "a one-off must not repeat"
            );
        }
        crate::topology_runtime::force_release(&tree_id).await;
    }

    /// `/clear`, `/resume`, fork and rebind all replace `config.session`
    /// inside one attach. The push cache is keyed on the session it resolved
    /// for, so the successor session re-resolves and §5.10 live updates keep
    /// arriving instead of dying silently for the rest of the attach.
    #[tokio::test]
    async fn a_session_switch_re_resolves_the_tree_and_pushes_keep_arriving() {
        let root = tempfile::tempdir().unwrap();
        let paths = MemoryRows::new();
        let mut first = SessionRecord::new(root.path().to_path_buf());
        let first_tree = durable_tree_id(&paths, &first);
        crate::topology_runtime::insert_root(
            first_tree.clone(),
            crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
                &first_tree,
                "root",
                "owner",
                root.path().to_path_buf(),
                "selected-tuple",
                crate::agent_tree_protocol::WriterMode::SingleLease,
            )
            .unwrap(),
            None,
        )
        .unwrap();
        let (events, rx) = host_event_sink();
        let mut push = TreeSnapshotPush::default();
        let home = SessionHome::new(std::env::temp_dir().join("plak-tree-test-sessions"));
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut first,
            &mut push,
            &events,
            false,
        );
        assert!(matches!(
            rx.try_iter().collect::<Vec<_>>().as_slice(),
            [HostEvent::TopologyNavigatorSnapshot(_)]
        ));

        // The operator runs `/clear`: a new session, a new tree, same attach.
        let mut second = SessionRecord::new(root.path().to_path_buf());
        let second_tree = durable_tree_id(&paths, &second);
        assert_ne!(first_tree, second_tree);
        let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            &second_tree,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap();
        tree.spawn_agent(
            "root",
            &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
        crate::topology_runtime::insert_root(second_tree.clone(), tree, None).unwrap();

        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut second,
            &mut push,
            &events,
            false,
        );
        let observed: Vec<HostEvent> = rx.try_iter().collect();
        let [HostEvent::TopologyNavigatorSnapshot(snapshot)] = observed.as_slice() else {
            panic!("a session switch must not silence the canvas: {observed:?}");
        };
        assert_eq!(snapshot.tree_id, second_tree);
        assert_eq!(snapshot.nodes.len(), 2);
        crate::topology_runtime::force_release(&first_tree).await;
        crate::topology_runtime::force_release(&second_tree).await;
    }
}

#[cfg(test)]
mod progress_rate_tests {
    #[test]
    fn material_progress_snapshots_are_rate_limited_without_sleeping() {
        let now = std::time::Instant::now();
        let next = now + std::time::Duration::from_secs(1);
        assert!(!super::snapshot_due(Some(1), 1, true, Some(next), now));
        assert!(super::snapshot_due(Some(1), 1, true, Some(next), next));
        assert!(!super::snapshot_due(Some(1), 1, false, Some(next), next));
        assert!(super::snapshot_due(Some(1), 2, false, Some(next), now));
    }
}
