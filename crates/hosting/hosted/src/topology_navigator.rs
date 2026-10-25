//! Host projection of the inert TUI tree navigator.
//!
//! The TUI never opens an Agent or store. This module snapshots `ProductAgentTree`
//! in deterministic preorder. Missing hub fails closed.

use crate::surface_protocol::{HostEvent, UserAction};
use plakous::model::Actor;
use plakous::sessions::{
    SESSION_HISTORY_VISIBILITY_REVISION, SessionHistoryDirection, SessionHistoryPageRequest,
    SessionHistoryVisibility, SessionLocator,
};
use plakous_host_protocol::views::transcript::{
    RowKey, TranscriptItem as KeyedItem, TranscriptScope,
};
use plakous_host_protocol::views::{PresentationSnapshot, TreeView};

use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::AttemptScopedRequest;
use crate::sessions::SessionRecord;
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::topology_runtime::ProductTopologyRuntime;

const HISTORY_PAGE_MAX_ITEMS: u32 = 32;
const HISTORY_PAGE_MAX_BYTES: u32 = 4096;

/// Shell loops with no attached hub: fail closed, never `load_tree`.
pub(crate) fn emit_navigator_unavailable<P: plakous_host_protocol::ProtocolExtension>(
    send: impl Fn(HostEvent<P>),
    reason: &str,
) {
    send(HostEvent::TopologyNavigatorUnavailable {
        reason: reason.to_owned(),
    });
}

fn tree_id_for_session(rows: &dyn crate::runtime::TopologyRows, session: &SessionRecord) -> String {
    crate::runtime::durable_topology_tree_id(rows, session)
}

/// Dispatch one navigator intent against the process hub. Never `load_tree`.
pub(crate) async fn dispatch_topology_navigator_action<
    P: plakous_host_protocol::ProtocolExtension,
>(
    rows: &dyn crate::runtime::TopologyRows,
    session: &SessionRecord,
    send: impl Fn(HostEvent<P>),
    action: &UserAction<P>,
) {
    let tree_id = tree_id_for_session(rows, session);
    let Some(runtime) = crate::topology_runtime::lookup(&tree_id) else {
        match action {
            UserAction::OpenAgentTranscript { agent_id } => {
                send(HostEvent::PresentationSnapshot(PresentationSnapshot {
                    scope: TranscriptScope::Agent(agent_id.clone()),
                    items: vec![KeyedItem {
                        key: RowKey::TRUNCATION,
                        row: TranscriptRow::info(
                            NoticeScope::Tree,
                            "no agent tree is attached to this session",
                        ),
                    }],
                    ..PresentationSnapshot::default()
                }));
                return;
            }
            UserAction::CloseAgentTranscript => return,
            _ => {}
        }
        // `ResumeTopologyTree` is an explicit operator intent (spec 02 §5.7 `r`,
        // §10.4) — never part of an attach sequence — so a hub
        // miss is reported like any other, not swallowed.
        emit_navigator_unavailable(
            send,
            "agent tree navigator unavailable: no topology hub is attached",
        );
        return;
    };
    match action {
        // A refresh re-emits the current revision even when it has not advanced,
        // so a dropped push is always recoverable by hand.
        UserAction::OpenTopologyNavigator | UserAction::RefreshTopologyTree => {
            emit_snapshot(&runtime, session, &send, true);
        }
        UserAction::ResumeTopologyTree => {
            resume_tree(session, &runtime, &send);
            // The caller starts the tree's background recovery pass.
            crate::agent::rejoin_inflight_after_resume(&tree_id).await;
        }
        UserAction::InterruptTopologyNode { agent_id } => {
            interrupt_node(session, &runtime, &send, agent_id).await;
        }
        UserAction::CloseTopologyNode { agent_id } => {
            close_node(session, &runtime, &send, agent_id).await;
        }
        UserAction::StopAllTopology => stop_all(session, &runtime, &send).await,
        _ => {}
    }
}

/// Repoints the canvas root's `Bind` line at the *attached session's* binding
///.
///
/// A durable tree is shared by every session in its lineage, and a rebind
/// rewrites the tree root's binding for its successor epoch. Resuming the
/// source session therefore found the canvas reading `claude-opus-5 · low`
/// while the header and composer read `codex · gpt-5.6-luna · low`. The tree
/// record is right about the tree; the canvas is showing *this* session, so the
/// session is bind authority for its own root row. Child rows are untouched:
/// their binding is the roster pin frozen when they spawned.
pub(crate) fn apply_session_root_binding(snapshot: &mut TreeView, session: &SessionRecord) {
    let (Some(account), Some(model)) = (session.account.as_deref(), session.model.as_deref())
    else {
        return;
    };
    let Some(root) = snapshot
        .nodes
        .iter_mut()
        .find(|node| node.parent_agent_id.is_none())
    else {
        return;
    };
    account.clone_into(&mut root.model_account);
    model.clone_into(&mut root.model_tuple);
    root.model_effort = session.effort.clone();
}

/// The tree's own typed view. The JSONL wire and the TUI's canvas adapter are
/// the two readers, and each chooses its own spelling from these enums.
pub fn project_snapshot(tree: &ProductAgentTree) -> TreeView {
    crate::agent_tree::view::project_tree_view(tree)
}

const HUB_POISONED: &str = "agent tree navigator unavailable: hub lock is poisoned";

/// Reads the tree for one navigator frame, reporting the hub as unavailable
/// rather than blocking or panicking when the mutex is poisoned.
fn read_tree<R, P: plakous_host_protocol::ProtocolExtension>(
    runtime: &ProductTopologyRuntime,
    send: impl Fn(HostEvent<P>),
    work: impl FnOnce(&ProductAgentTree) -> R,
) -> Option<R> {
    let Ok(result) = runtime.read(work) else {
        emit_navigator_unavailable(send, HUB_POISONED);
        return None;
    };
    Some(result)
}

/// One navigator mutation: applied under the mutex, published, and whatever
/// it queued settled — the navigator never holds the tree itself, so it
/// cannot strand an effect.
fn mutate_tree<R, P: plakous_host_protocol::ProtocolExtension>(
    runtime: &ProductTopologyRuntime,
    send: impl Fn(HostEvent<P>) + Copy,
    what: &str,
    work: impl FnOnce(&mut ProductAgentTree) -> Result<R, String>,
) -> Option<R> {
    match runtime.mutate_then_emit(false, work) {
        Ok(result) => Some(result),
        Err(error) => {
            send(HostEvent::Row(TranscriptRow::error(
                NoticeScope::Tree,
                format!("{what}: {error}"),
            )));
            None
        }
    }
}

fn emit_snapshot<P: plakous_host_protocol::ProtocolExtension>(
    runtime: &ProductTopologyRuntime,
    session: &SessionRecord,
    send: impl Fn(HostEvent<P>) + Copy,
    solicited: bool,
) {
    let Some(mut snapshot) = read_tree(runtime, send, project_snapshot) else {
        return;
    };
    apply_session_root_binding(&mut snapshot, session);
    snapshot.solicited = solicited;
    send(HostEvent::TopologyNavigatorSnapshot(snapshot));
}

fn resume_tree<P: plakous_host_protocol::ProtocolExtension>(
    session: &SessionRecord,
    runtime: &ProductTopologyRuntime,
    send: impl Fn(HostEvent<P>) + Copy,
) {
    // Only a reopened tree is paused. Resuming anything else would fault this
    // process's own in-flight seals, adopts and removals as if a lost process
    // had left them, so a live tree is left alone and the operator told so.
    let Some(paused) = read_tree(runtime, send, |tree| {
        tree.state == crate::agent_tree_protocol::TreeAdmissionState::RestoredPaused
    }) else {
        return;
    };
    if !paused {
        send(HostEvent::Row(TranscriptRow::info(
            NoticeScope::Tree,
            "agent tree is not paused; nothing to resume",
        )));
    } else if mutate_tree(runtime, send, "tree resume failed", |tree| {
        tree.resume_tree().map_err(|error| error.message)
    })
    .is_none()
    {
        return;
    }
    emit_snapshot(runtime, session, send, true);
}

/// One committed history entry reduced to what the rehearsal prints.
struct TranscriptItem {
    actor: String,
    text: String,
    blocks: usize,
    /// Tool blocks in canonical order: the call identity, and the exact tool
    /// name when this block is the call rather than its result.
    tools: Vec<(String, Option<String>)>,
}

/// One printed line of a child page: an actor label and its text.
#[derive(Debug)]
struct TranscriptLine {
    actor: String,
    text: String,
}

fn lines_from_items(items: Vec<plakous::sessions::SessionHistoryItem>) -> Vec<TranscriptLine> {
    project_transcript_lines(items.into_iter().map(|item| {
        TranscriptItem {
            actor: transcript_actor_label(&item).to_owned(),
            text: item.display_text().unwrap_or("").to_owned(),
            blocks: item.block_count(),
            tools: item
                .tool_blocks()
                .iter()
                .map(|block| {
                    (
                        block.call_id().to_string(),
                        block.tool().map(|tool| tool.as_str().to_owned()),
                    )
                })
                .collect(),
        }
    }))
}

/// Repository-only bounded snapshot of a child's first committed history page,
/// read offline; no live child attachment is attempted.
#[doc(hidden)]
pub fn rehearse_child_presentation_snapshot(
    items: Vec<plakous::sessions::SessionHistoryItem>,
) -> PresentationSnapshot {
    let transcript = lines_from_items(items)
        .into_iter()
        .map(|line| match line.actor.as_str() {
            "user" => TranscriptRow::user(line.text),
            "assistant" => TranscriptRow::assistant(line.text),
            actor => TranscriptRow::info(NoticeScope::Tree, format!("{actor}: {}", line.text)),
        })
        .collect::<Vec<_>>();
    crate::runtime::rehearse_presentation_snapshot(&transcript)
}

/// Projects one committed child page as inert lines.
///
/// One history entry is not one line: an assistant entry carries its text and
/// the tool calls it made, and a tool entry carries results. Each block gets
/// its own line with a subject, and an entry that carries only structure says
/// so instead of printing an empty line.
///
/// Result rows resolve their tool name from the calls on the same page; a call
/// paged earlier leaves the row named `result`, never a guess. This is a view:
/// it names blocks, never their arguments or results.
fn project_transcript_lines(
    items: impl IntoIterator<Item = TranscriptItem>,
) -> Vec<TranscriptLine> {
    let mut called: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut lines = Vec::new();
    for item in items {
        let before = lines.len();
        if let Some(text) = mail_row(&item.text) {
            lines.push(TranscriptLine {
                actor: "mail".to_owned(),
                text,
            });
        } else {
            for text in item.text.lines() {
                lines.push(TranscriptLine {
                    actor: item.actor.clone(),
                    text: text.to_owned(),
                });
            }
        }
        for (call_id, tool) in item.tools {
            let text = match tool {
                Some(tool) => {
                    called.insert(call_id, tool.clone());
                    tool
                }
                None => called
                    .get(&call_id)
                    .map_or_else(|| "result".to_owned(), |tool| format!("{tool} · result")),
            };
            lines.push(TranscriptLine {
                actor: "tool".to_owned(),
                text,
            });
        }
        if lines.len() == before && item.blocks > 0 {
            lines.push(TranscriptLine {
                actor: item.actor,
                text: format!("· {} blocks without display text", item.blocks),
            });
        }
    }
    lines
}

/// Purposes a host-authored mail envelope may carry. `result` is
/// host-authored and `followup` re-tasks a finished node; the other four are
/// the unchanged model-authored purposes.
const MAIL_PURPOSES: &[&str] = &[
    "result",
    "followup",
    "coordinate",
    "finding",
    "question",
    "reply",
];

/// Collapses one delivered mail envelope into a single transcript row.
///
/// A `result` push lands in the parent's session as an ordinary
/// entry whose text is the whole envelope — sender, attempt, candidate, and the
/// child's `output_json`. Printed line by line that is a raw mail dump that
/// buries the transcript; the operator needs *who, which attempt, which
/// candidate, and what it said*, which is one row. Every other purpose renders
/// the same way with its own name, so a `followup` reads as mail and not as a
/// user turn the root typed.
///
/// Returns `None` for anything that is not an envelope, which is every ordinary
/// entry: a body the host did not author is never reshaped.
pub(crate) fn mail_row(text: &str) -> Option<String> {
    let envelope: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let object = envelope.as_object()?;
    let purpose = object.get("purpose")?.as_str()?;
    if !MAIL_PURPOSES.contains(&purpose) {
        return None;
    }
    let from = object.get("from")?.as_str()?;
    let mut row = format!("{purpose} · {}", short_agent(from));
    if let Some(seq) = object
        .get("attempt_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|attempt| attempt.rsplit_once("-a").map(|(_, seq)| seq.to_owned()))
    {
        row.push_str(&format!(" · a{seq}"));
    }
    if let Some(candidate) = object
        .get("candidate")
        .and_then(serde_json::Value::as_str)
        .filter(|candidate| !candidate.is_empty())
    {
        row.push_str(&format!(" · {}", short_agent(candidate)));
    }
    let body = object
        .get("output")
        .or_else(|| object.get("output_json"))
        .or_else(|| object.get("body"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if let Some(body) = crate::runtime::bounded_notice_text(body, 2048) {
        row.push_str(&format!(" · {body}"));
    }
    Some(row)
}

/// Splits a root command that carries a child-mail batch back into the
/// operator's own text and one row per `<child_message>` block, each read like
/// [`mail_row`]: purpose, sender and the bounded body (the TUI folds its presentation).
///
/// The batch is appended after the operator's text as the fixed frame
/// sentence and escaped blocks (`result_mail::select`), so the boundary is the
/// last frame whose remainder parses as whole blocks to the end: operator text
/// may carry the frame and even an unterminated block before it, and an
/// escaped body cannot contain a `<child_message` opener. Returns `None` for
/// text that carries no batch.
pub(crate) fn split_child_mail(text: &str) -> Option<(String, Vec<String>)> {
    let frame = format!("\n\n{}", crate::runtime::result_mail::CHILD_MAIL_FRAME);
    text.rmatch_indices(&frame).find_map(|(start, _)| {
        let rows = child_mail_rows(&text[start + frame.len()..])?;
        Some((text[..start].to_owned(), rows))
    })
}

fn child_mail_rows(mut rest: &str) -> Option<Vec<String>> {
    let mut rows = Vec::new();
    while !rest.is_empty() {
        let block = rest.strip_prefix("\n\n<child_message message_id=\"")?;
        let (_, block) = block.split_once("\" from=\"")?;
        let (from, block) = block.split_once("\" purpose=\"")?;
        let (purpose, block) = block.split_once("\">\n")?;
        // Bodies are escaped, so the first closing tag ends this block.
        let (body, after) = block.split_once("\n</child_message>")?;
        let mut row = format!("{purpose} · {}", short_agent(&unescape_mail(from)));
        if let Some(body) = crate::runtime::bounded_notice_text(&unescape_mail(body), 2048) {
            row.push_str(&format!(" · {body}"));
        }
        rows.push(row);
        rest = after;
    }
    (!rows.is_empty()).then_some(rows)
}

fn unescape_mail(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

/// Trailing segment of a durable id, matching what the canvas prints.
fn short_agent(id: &str) -> String {
    let tail = id.rsplit('-').next().unwrap_or(id);
    format!("#{}", if tail.is_empty() { id } else { tail })
}

async fn interrupt_node<P: plakous_host_protocol::ProtocolExtension>(
    session: &SessionRecord,
    runtime: &ProductTopologyRuntime,
    send: impl Fn(HostEvent<P>) + Copy,
    agent_id: &str,
) {
    let operation_id = uuid::Uuid::new_v4().to_string();
    if mutate_tree(runtime, send, "interrupt failed", |tree| {
        let root = tree.root_agent_id.clone();
        tree.interrupt_agent(
            &root,
            &AttemptScopedRequest {
                agent_id: agent_id.to_owned(),
                expected_attempt_id: None,
            },
            Some(&operation_id),
        )
        .map(|_| ())
        .map_err(|error| error.message)
    })
    .is_none()
    {
        return;
    }
    if let Err(message) = runtime.cancel_live(agent_id).await {
        send(HostEvent::Row(TranscriptRow::error(
            NoticeScope::Tree,
            format!("node interrupted; live cancel failed: {message}"),
        )));
    }
    emit_snapshot(runtime, session, send, false);
}

async fn close_node<P: plakous_host_protocol::ProtocolExtension>(
    session: &SessionRecord,
    runtime: &ProductTopologyRuntime,
    send: impl Fn(HostEvent<P>) + Copy,
    agent_id: &str,
) {
    let operation_id = uuid::Uuid::new_v4().to_string();
    let writer = runtime.observer();
    let Some(closed) = mutate_tree(runtime, send, "close failed", |tree| {
        let root = tree.root_agent_id.clone();
        let closed = tree
            .close_agent(
                &root,
                &AttemptScopedRequest {
                    agent_id: agent_id.to_owned(),
                    expected_attempt_id: None,
                },
                Some(&operation_id),
            )
            .map_err(|error| error.message)?;
        for (id, status) in &closed {
            let detach = crate::agent_tree::NodeDetach::from_node_status(*status)
                .unwrap_or(crate::agent_tree::NodeDetach::Closed);
            tree.try_emit_tree_node_detached(writer.as_ref(), id, detach);
        }
        Ok(closed)
    }) else {
        return;
    };
    let closed_ids: Vec<String> = closed.into_iter().map(|(id, _)| id).collect();
    let cancel = runtime.cancel_live_many(&closed_ids).await;
    if !cancel.is_clean() {
        let message = cancel
            .first_failure
            .unwrap_or_else(|| "child turn could not be cancelled".to_owned());
        let _ = mutate_tree(
            runtime,
            send,
            "retirement failure could not be persisted",
            |tree| {
                for owner in &cancel.owner_failures {
                    let id = &owner.agent_id;
                    if !tree
                        .record_host_retirement_for_attempt(
                            id,
                            owner.expected_attempt_id.as_ref(),
                            owner.message.clone(),
                        )
                        .and_then(|value| runtime.driver().apply_recorded(tree, id).map(|()| value))
                        .map_err(|error| error.message)?
                    {
                        continue;
                    }
                    tree.try_emit_tree_node_detached(
                        writer.as_ref(),
                        id,
                        crate::agent_tree::NodeDetach::RecoveryRequired,
                    );
                }
                Ok(())
            },
        );
        if !cancel.owner_failures.is_empty() {
            runtime.driver().request_retirement_recovery();
        }
        send(HostEvent::Row(TranscriptRow::error(
            NoticeScope::Tree,
            format!(
                "close applied; live cancel failed for {}: {message}",
                cancel
                    .failed
                    .iter()
                    .chain(&cancel.joiner_expired)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        )));
    }
    emit_snapshot(runtime, session, send, false);
}

async fn stop_all<P: plakous_host_protocol::ProtocolExtension>(
    session: &SessionRecord,
    runtime: &ProductTopologyRuntime,
    send: impl Fn(HostEvent<P>) + Copy,
) {
    let operation_id = uuid::Uuid::new_v4().to_string();
    let writer = runtime.observer();
    let Some(closed) = mutate_tree(runtime, send, "stop-all failed", |tree| {
        let root = tree.root_agent_id.clone();
        let closed = tree
            .stop_all(&root, Some(&operation_id))
            .map_err(|error| error.message)?;
        for (id, status) in &closed {
            let detach = crate::agent_tree::NodeDetach::from_node_status(*status)
                .unwrap_or(crate::agent_tree::NodeDetach::Closed);
            tree.try_emit_tree_node_detached(writer.as_ref(), id, detach);
        }
        Ok(closed)
    }) else {
        return;
    };
    let closed_ids: Vec<String> = closed.into_iter().map(|(id, _)| id).collect();
    let cancel = runtime.cancel_live_many(&closed_ids).await;
    if !cancel.is_clean() {
        let message = cancel
            .first_failure
            .unwrap_or_else(|| "child turn could not be cancelled".to_owned());
        let _ = mutate_tree(
            runtime,
            send,
            "retirement failure could not be persisted",
            |tree| {
                for owner in &cancel.owner_failures {
                    let id = &owner.agent_id;
                    if !tree
                        .record_host_retirement_for_attempt(
                            id,
                            owner.expected_attempt_id.as_ref(),
                            owner.message.clone(),
                        )
                        .and_then(|value| runtime.driver().apply_recorded(tree, id).map(|()| value))
                        .map_err(|error| error.message)?
                    {
                        continue;
                    }
                    tree.try_emit_tree_node_detached(
                        writer.as_ref(),
                        id,
                        crate::agent_tree::NodeDetach::RecoveryRequired,
                    );
                }
                Ok(())
            },
        );
        if !cancel.owner_failures.is_empty() {
            runtime.driver().request_retirement_recovery();
        }
        send(HostEvent::Row(TranscriptRow::error(
            NoticeScope::Tree,
            format!(
                "stop-all applied; live cancel failed for {}: {message}",
                cancel
                    .failed
                    .iter()
                    .chain(&cancel.joiner_expired)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        )));
    }
    emit_snapshot(runtime, session, send, false);
}

/// The first committed page of a child's history, for the rehearsal's
/// offline read.
#[doc(hidden)]
#[must_use]
pub fn initial_history_page_request(locator: SessionLocator) -> SessionHistoryPageRequest {
    SessionHistoryPageRequest::new(
        locator,
        locator.checkpoint_id(),
        SessionHistoryVisibility::Content,
        SESSION_HISTORY_VISIBILITY_REVISION,
        SessionHistoryDirection::Forward,
        HISTORY_PAGE_MAX_ITEMS,
        HISTORY_PAGE_MAX_BYTES,
        None,
    )
}

/// The label a paged transcript row carries.
///
/// A harness note is `Actor::User` because a steering segment must lower as a
/// user-role message on every protocol, so labelling by actor alone would
/// present text the harness injected mid-turn as something the operator typed.
/// Provenance is what separates them.
fn transcript_actor_label(item: &plakous::sessions::SessionHistoryItem) -> &'static str {
    transcript_actor_label_for(item.actor(), item.is_harness_authored())
}

const fn transcript_actor_label_for(actor: Actor, harness_authored: bool) -> &'static str {
    if harness_authored {
        return "harness";
    }
    actor_label(actor)
}

const fn actor_label(actor: Actor) -> &'static str {
    match actor {
        Actor::Developer => "developer",
        Actor::System => "system",
        Actor::User => "user",
        Actor::Assistant => "assistant",
        Actor::Tool => "tool",
        Actor::Harness => "harness",
        _ => "other",
    }
}

#[cfg(test)]
mod tests;
