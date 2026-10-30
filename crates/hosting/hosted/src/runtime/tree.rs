//! Topology tree bridging: lineage, snapshots, and sync dispatch.

use crate::session_log::SessionEvent;
use crate::surface_protocol::ContentEvent;
use plakous::application::ApplicationExecutor;

use crate::sessions::SessionRecord;

use super::rebind::durable_tree_id;
use super::{Sender, TopologyRows, TreeSnapshotPush, TurnProjection, UserAction};
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
    pub(super) fn registered(rows: &dyn TopologyRows, session: &SessionRecord) -> Self {
        let mut push = Self::default();
        let _ = push.runtime(rows, session);
        push
    }

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
            self.retirement_faults = None;
            self.last_revision = None;
            self.notified_results.clear();
        }
        let cached = self
            .tree_id
            .get_or_insert_with(|| durable_tree_id(rows, session));
        if let Some(runtime) = crate::topology_runtime::lookup(cached) {
            self.retirement_faults
                .get_or_insert_with(|| runtime.observe_retirement_faults());
            return Some(runtime);
        }
        // A tree minted for this session *after* the lineage walk resolved an
        // ancestor's id: adopt it rather than pushing nothing for the attach.
        let runtime = crate::topology_runtime::lookup(&session.id)?;
        session.id.clone_into(cached);
        self.last_revision = None;
        self.retirement_faults = Some(runtime.observe_retirement_faults());
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
pub(crate) fn bounded_text(text: &str, bound: usize) -> Option<String> {
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

/// Derive every retained attempt's milestones, even if it finished before
/// the host first observed it. The notice id, not a process-local status
/// sample or the bounded transcript, owns exactly-once publication.
fn child_milestones(
    tree: &plakous_recursive_subagents::tree::ProductAgentTree,
    node: &plakous_recursive_subagents::tree::NodeRecord,
) -> Vec<(String, AgentMilestone)> {
    use plakous_host_protocol::topology::AttemptStatus;
    let mut attempts: Vec<_> = tree
        .attempts()
        .values()
        .filter(|attempt| attempt.agent_id == node.agent_id)
        .collect();
    attempts.sort_by_key(|attempt| attempt.turn_token);
    let mut rows = Vec::new();
    for attempt in attempts {
        let milestone = |event, text, template, duration_ms| AgentMilestone {
            agent_id: node.agent_id.clone(),
            event,
            template,
            text,
            duration_ms,
            candidate: None,
            files: None,
            paths: Vec::new(),
        };
        let id = |kind| {
            format!(
                "tree:{}:attempt:{}:{kind}",
                tree.tree_id, attempt.attempt_id
            )
        };
        let (start, kind) = if attempt.retry_of.is_some() {
            (MilestoneEvent::Retasked, "retasked")
        } else {
            (MilestoneEvent::Spawned, "spawned")
        };
        rows.push((
            id(kind),
            milestone(
                start,
                Some(node.description.clone()).filter(|text| !text.is_empty()),
                node.template.as_ref().map(ToString::to_string),
                None,
            ),
        ));
        let failure = attempt
            .failure
            .as_ref()
            .map(|failure| failure.message.as_str());
        let terminal = match attempt.status {
            AttemptStatus::Completed
            | AttemptStatus::CompletedWithoutSubmission
            | AttemptStatus::Adopted => Some((
                MilestoneEvent::Finished,
                "finished",
                attempt.output_json.as_deref().and_then(result_text),
            )),
            AttemptStatus::Failed | AttemptStatus::Blocked => Some((
                MilestoneEvent::Failed,
                "failed",
                failure.and_then(milestone_text),
            )),
            AttemptStatus::RecoveryRequired => Some((
                MilestoneEvent::NeedsRecovery,
                "needs_recovery",
                tree.fault_reason(&node.agent_id)
                    .or(failure)
                    .and_then(milestone_text),
            )),
            _ => None,
        };
        if let Some((event, kind, text)) = terminal {
            let duration = (event == MilestoneEvent::Finished)
                .then(|| {
                    attempt
                        .ended_at
                        .map(|ended| ended.saturating_sub(attempt.started_at))
                })
                .flatten();
            rows.push((id(kind), milestone(event, text, None, duration)));
        }
        if node.attempt_id.as_ref() == Some(&attempt.attempt_id)
            && node.status == plakous_host_protocol::topology::NodeStatus::RecoveryRequired
            && attempt.status != AttemptStatus::RecoveryRequired
        {
            // A checkout fault can require recovery after the attempt ended.
            rows.push((
                id("needs_recovery"),
                milestone(
                    MilestoneEvent::NeedsRecovery,
                    tree.fault_reason(&node.agent_id)
                        .or(failure)
                        .and_then(milestone_text),
                    None,
                    None,
                ),
            ));
        }
    }
    rows
}

fn adoption_notices(
    tree: &plakous_recursive_subagents::tree::ProductAgentTree,
) -> Vec<(String, TranscriptRow)> {
    tree.operations()
        .values()
        .filter(|operation| {
            operation.caller_agent_id == tree.root_agent_id
                && operation.tool_name == "adopt_candidate"
                && operation.terminal_code.is_none()
        })
        .filter_map(|operation| {
            let receipt: plakous_host_protocol::topology::AdoptOutcome =
                serde_json::from_str(operation.terminal_response.as_deref()?).ok()?;
            if receipt.reverted {
                return Some((
                    format!(
                        "tree:{}:revert:{}:{}",
                        tree.tree_id, receipt.candidate_id, operation.operation_id
                    ),
                    TranscriptRow::info(
                        NoticeScope::Tree,
                        format!(
                            "reverted {} · {} {}",
                            receipt.candidate_id,
                            receipt.files_changed,
                            if receipt.files_changed == 1 {
                                "file"
                            } else {
                                "files"
                            }
                        ),
                    ),
                ));
            }
            let source = tree.candidates().get(&receipt.candidate_id)?;
            let milestone = super::tool_facts::adopt_milestone(
                receipt.candidate_id.as_str(),
                plakous_recursive_subagents::call_facts::TopologyCallFacts::Adopted {
                    files_changed: receipt.files_changed,
                },
                |_| {
                    Some((
                        source.producing_agent_id.clone(),
                        tree.nodes()
                            .get(&source.producing_agent_id)
                            .and_then(|node| node.template.as_ref())
                            .map(ToString::to_string),
                    ))
                },
            )?;
            Some((
                format!(
                    "tree:{}:adopt:{}:{}",
                    tree.tree_id, receipt.candidate_id, operation.operation_id
                ),
                TranscriptRow::Milestone(milestone),
            ))
        })
        .collect()
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
#[allow(
    clippy::too_many_lines,
    reason = "Tree snapshot publication reconciles revisions, child attention, adoption notices, and navigator state before emitting one view."
)]
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
    if let Some(faults) = &mut push.retirement_faults {
        loop {
            match faults.try_recv() {
                Ok((_, message)) => {
                    let _ = events.notice(TranscriptRow::error(NoticeScope::Tree, message));
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
                    tracing::warn!(target: "plak.topology", count, "retirement notice observer lagged");
                }
                Err(
                    tokio::sync::broadcast::error::TryRecvError::Empty
                    | tokio::sync::broadcast::error::TryRecvError::Closed,
                ) => break,
            }
        }
    }
    if tokio::runtime::Handle::try_current().is_ok() {
        push.child_live.sync(&runtime);
    }
    let Ok(mut notice_ids) = known_notice_ids(store, session) else {
        return;
    };
    let child_live = &mut push.child_live;
    let last_revision = &mut push.last_revision;
    let next_progress_snapshot = &mut push.next_progress_snapshot;
    let notified_results = &mut push.notified_results;
    let announced_abandoned = &mut push.announced_abandoned_turns;
    let (milestones, result_ids, results) = runtime
        .read(|tree| {
            let mut rows = Vec::new();
            for node in tree.nodes().values() {
                if node.agent_id == tree.root_agent_id {
                    continue;
                }
                rows.extend(
                    child_milestones(tree, node)
                        .into_iter()
                        .map(|(id, milestone)| (id, TranscriptRow::Milestone(milestone))),
                );
            }
            for question in root_questions(tree) {
                rows.push((
                    format!("tree:{}:question:{}", tree.tree_id, question.message_id),
                    TranscriptRow::Milestone(ask_milestone(tree, question)),
                ));
            }
            rows.extend(adoption_notices(tree));
            let results = tree
                .messages()
                .values()
                .filter(|mail| {
                    mail.to == tree.root_agent_id
                        && mail.purpose == crate::agent_tree_protocol::MessagePurpose::Result
                })
                .collect::<Vec<_>>();
            let result_ids = results
                .iter()
                .map(|mail| mail.message_id.clone())
                .collect::<std::collections::HashSet<_>>();
            let results = results
                .into_iter()
                .filter(|mail| !notified_results.contains(&mail.message_id))
                .map(|mail| {
                    (
                        format!("tree:{}:result:{}", tree.tree_id, mail.message_id),
                        mail.clone(),
                    )
                })
                .collect::<Vec<_>>();
            (rows, result_ids, results)
        })
        .unwrap_or_default();
    // No session feed fact implies a tree milestone, so each is an operator
    // notice intent (spec 02 §2.6) and a restore shows what the live face did.
    for (id, row) in milestones {
        let _ = publish_notice_once(store, home, session, events, &mut notice_ids, &id, row);
    }
    notified_results.retain(|id| result_ids.contains(id));
    for (notice_id, mail) in results {
        let recorded = notice_ids.contains(&notice_id);
        if recorded && mail.durable != crate::agent_tree::MailDurableState::Accepted {
            notified_results.insert(mail.message_id);
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
        if !recorded {
            let committed =
                super::admission::commit_session_update(store, home, session, |candidate| {
                    candidate
                        .transcript
                        .record_named_notice(notice_id.clone(), TranscriptRow::Tool(row));
                    true
                });
            if committed != Ok(true) {
                continue;
            }
            notice_ids.insert(notice_id);
        }
        if events.send(ContentEvent::ToolLine {
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
        let _ = events.send(ContentEvent::TopologyNavigatorSnapshot(snapshot));
    });
}

pub(super) fn publish_notice_once<P: plakous_host_protocol::ProtocolExtension>(
    store: &dyn crate::session_store::SessionRecordStore,
    home: &crate::session_home::SessionHome,
    session: &mut SessionRecord,
    events: &Sender<P>,
    notice_ids: &mut std::collections::HashSet<String>,
    id: &str,
    row: TranscriptRow,
) -> Result<bool, String> {
    if notice_ids.contains(id)
        || session
            .transcript
            .pending_notice_entries()
            .any(|(pending, _)| pending == Some(id))
    {
        notice_ids.insert(id.into());
        return Ok(false);
    }
    let committed = super::admission::commit_session_update(store, home, session, |candidate| {
        candidate
            .transcript
            .record_named_notice(id.into(), row.clone());
        true
    })?;
    if committed {
        notice_ids.insert(id.into());
        let _ = events.send(ContentEvent::Row(row));
    }
    Ok(committed)
}

pub(super) fn known_notice_ids(
    store: &dyn crate::session_store::SessionRecordStore,
    session: &SessionRecord,
) -> Result<std::collections::HashSet<String>, String> {
    let mut ids: std::collections::HashSet<_> = match store.load_intents(&session.id) {
        Ok(intents) => intents
            .notices
            .into_iter()
            .map(|notice| notice.notice_id)
            .collect(),
        Err(_) if session.store_revision == 0 => std::collections::HashSet::new(),
        Err(error) => return Err(error),
    };
    ids.extend(
        session
            .transcript
            .pending_notice_entries()
            .filter_map(|(id, _)| id.map(str::to_owned)),
    );
    Ok(ids)
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
            let _ = events.send(ContentEvent::TopologyNavigatorSnapshot(snapshot));
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
            finished.send(ContentEvent::Row(row))
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
    if matches!(action, UserAction::CloseAgentTranscript) {
        push.child_live.close_agent_transcript(events);
        return;
    }
    if let Err(error) = executor.block_on(async {
        crate::topology_navigator::dispatch_topology_navigator_action(
            rows, session, events, action,
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
                let _ = events.send(ContentEvent::ToolInvocationOrigin {
                    call_id: receipt.call_id.clone(),
                    command_id: command_id.to_owned(),
                });
            }
            let kind = crate::activity::tool_activity_kind(&receipt.name);
            let _ = events.send(ContentEvent::ToolLine {
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
            let _ = events.send(ContentEvent::ToolLine {
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
mod tests;

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
