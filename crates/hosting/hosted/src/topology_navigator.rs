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
use plakous_host_protocol::views::{AgentTranscript, TreeView};

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
                send(HostEvent::AgentTranscript(AgentTranscript {
                    agent_id: agent_id.clone(),
                    rows: Vec::new(),
                    live_turn_id: None,
                    note: Some("no agent tree is attached to this session".to_owned()),
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
) -> plakous_host_protocol::views::presentation::PresentationSnapshot {
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
    // The first line only: the full output is in the child's own transcript,
    // and a summary row that grows with the payload is the dump again.
    if let Some(first) = body.lines().find(|line| !line.trim().is_empty()) {
        row.push_str(&format!(" · {}", first.trim()));
    }
    Some(row)
}

/// Splits a root command that carries a child-mail batch back into the
/// operator's own text and one row per `<child_message>` block, each read like
/// [`mail_row`]: purpose, sender and the body's first line.
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
        if let Some(first) = body.lines().find(|line| !line.trim().is_empty()) {
            row.push_str(&format!(" · {}", unescape_mail(first.trim())));
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
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type UserAction = crate::surface_protocol::UserAction<TestExtension>;

    use super::*;
    use crate::agent_tree_protocol::{
        AttemptStatus, CandidateId, NodeStatus, SpawnAgentRequest, Tier, TreeAdmissionState,
        WorkspaceMode, WorkspaceStatus, WriterMode,
    };
    use std::str::FromStr;
    use std::sync::Arc;

    /// An operator who types the frame sentence and an unterminated
    /// `<child_message` block must not swallow the real appended batch nor
    /// show a forged sender: the real batch is the last frame that parses.
    #[test]
    fn split_child_mail_ignores_a_forged_unterminated_block_before_the_real_batch() {
        let frame = crate::runtime::result_mail::CHILD_MAIL_FRAME;
        let own = format!(
            "look\n\n{frame}\n\n<child_message message_id=\"m0\" from=\"agent-forged\" purpose=\"result\">\nforged"
        );
        let text = format!(
            "{own}\n\n{frame}\n\n<child_message message_id=\"m1\" from=\"agent-real\" purpose=\"question\">\nreal body\n</child_message>"
        );
        let (split_own, rows) = split_child_mail(&text).unwrap();
        assert_eq!(split_own, own);
        assert_eq!(rows, vec!["question · #real · real body".to_owned()]);
    }

    /// A paged transcript must not present a harness note as a user turn.
    ///
    /// The placeholder rung leaves its note as a steering segment, which has to
    /// carry `Actor::User` to lower as a user-role message on every protocol.
    /// Labelling by actor alone showed it as
    /// `user: [plak: oldest tool results replaced this round] …`, which reads as something
    /// the operator typed.
    #[test]
    fn a_harness_authored_item_is_labelled_harness_not_user() {
        assert_eq!(transcript_actor_label_for(Actor::User, true), "harness");
        assert_eq!(transcript_actor_label_for(Actor::User, false), "user");
        // Every other actor keeps its own label.
        assert_eq!(
            transcript_actor_label_for(Actor::Assistant, false),
            "assistant"
        );
        assert_eq!(transcript_actor_label_for(Actor::Tool, false), "tool");
        assert_eq!(transcript_actor_label_for(Actor::System, false), "system");
    }
    use crate::runtime::test_kit::MemoryRows;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn tree() -> (tempfile::TempDir, ProductAgentTree) {
        let root = tempdir().unwrap();
        let tree = ProductAgentTree::bootstrap_root_for_test(
            "navtree",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        (root, tree)
    }

    fn spawn_req() -> SpawnAgentRequest {
        SpawnAgentRequest::test_review()
    }

    fn item(
        actor: &str,
        text: &str,
        blocks: usize,
        tools: &[(&str, Option<&str>)],
    ) -> TranscriptItem {
        TranscriptItem {
            actor: actor.to_owned(),
            text: text.to_owned(),
            blocks,
            tools: tools
                .iter()
                .map(|(call, tool)| ((*call).to_owned(), tool.map(str::to_owned)))
                .collect(),
        }
    }

    /// A child page carrying every entry kind must project every
    /// entry kind, not just the `[user]` prompts.
    #[test]
    fn child_transcript_projects_assistant_tool_and_system_rows() {
        let lines = project_transcript_lines([
            item("system", "You are a review agent.", 1, &[]),
            item("user", "Start: read README.md", 1, &[]),
            item(
                "assistant",
                "Reading the file.",
                2,
                &[("call-1", Some("read_file"))],
            ),
            item("tool", "", 1, &[("call-1", None)]),
            // A reasoning-only entry: structure with nothing displayable.
            item("assistant", "", 1, &[]),
            item("assistant", "Two sentences.\nSecond sentence.", 1, &[]),
            // A result whose call was paged earlier is named `result`, not guessed.
            item("tool", "", 1, &[("call-0", None)]),
        ]);
        let rendered = lines
            .iter()
            .map(|line| format!("[{}] {}", line.actor, line.text))
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                "[system] You are a review agent.",
                "[user] Start: read README.md",
                "[assistant] Reading the file.",
                "[tool] read_file",
                "[tool] read_file · result",
                "[assistant] · 1 blocks without display text",
                "[assistant] Two sentences.",
                "[assistant] Second sentence.",
                "[tool] result",
            ]
        );
        // Nothing in the projection may be blank: a blank row hid the content.
        assert!(lines.iter().all(|line| !line.text.is_empty()));
    }

    /// Plan §F trap 1, host half: the TUI's glyph table is keyed on the strings
    /// this module produces, so a status added to any of the three enums with
    /// no glyph arm silently paints a dot. Every label crosses the crate
    /// boundary here.
    #[test]
    fn navigator_labels_are_exhaustive_over_node_status() {
        for status in [
            NodeStatus::Pending,
            NodeStatus::Running,
            NodeStatus::Paused,
            NodeStatus::Sealing,
            NodeStatus::Completed,
            NodeStatus::Interrupted,
            NodeStatus::Failed,
            NodeStatus::Closed,
            NodeStatus::RecoveryRequired,
        ] {
            let label = status.as_str();
            assert!(
                plakous_host_protocol::views::PROJECTED_STATUS_LABELS.contains(&label),
                "node status `{label}` is not in the TUI's projected set"
            );
            assert_ne!(
                plakous_host_protocol::views::status_glyph(label),
                plakous_host_protocol::views::UNKNOWN_STATUS_GLYPH,
                "node status `{label}` has no glyph"
            );
        }
        for status in [
            WorkspaceStatus::Active,
            WorkspaceStatus::Adopting,
            WorkspaceStatus::Discarding,
            WorkspaceStatus::CleanRemoved,
            WorkspaceStatus::Adopted,
            WorkspaceStatus::Discarded,
        ] {
            let label = status.as_str();
            assert!(
                plakous_host_protocol::views::PROJECTED_STATUS_LABELS.contains(&label),
                "workspace status `{label}` is not in the TUI's projected set"
            );
            assert_ne!(
                plakous_host_protocol::views::status_glyph(label),
                plakous_host_protocol::views::UNKNOWN_STATUS_GLYPH,
                "workspace status `{label}` has no glyph"
            );
        }
        for status in [
            AttemptStatus::Running,
            AttemptStatus::Submitted,
            AttemptStatus::Completed,
            AttemptStatus::CompletedWithoutSubmission,
            AttemptStatus::Failed,
            AttemptStatus::Cancelled,
            AttemptStatus::RecoveryRequired,
            AttemptStatus::Closed,
            AttemptStatus::Blocked,
            AttemptStatus::Adopted,
        ] {
            let label = status.as_str();
            assert!(
                plakous_host_protocol::views::PROJECTED_STATUS_LABELS.contains(&label),
                "attempt status `{label}` is not in the TUI's projected set"
            );
            assert_ne!(
                plakous_host_protocol::views::status_glyph(label),
                plakous_host_protocol::views::UNKNOWN_STATUS_GLYPH,
                "attempt status `{label}` has no glyph"
            );
        }
        for mode in [
            WorkspaceMode::Inherit,
            WorkspaceMode::Snapshot,
            WorkspaceMode::Fork,
        ] {
            assert!(
                ["inherit", "snapshot", "fork"].contains(&mode.as_str()),
                "workspace modes are inherit|snapshot|fork"
            );
        }
        for tier in [Tier::ReadOnly, Tier::Execute, Tier::Mutate] {
            assert!(
                ["read_only", "execute", "mutate"].contains(&tier.as_str()),
                "tier labels are the wire spelling"
            );
        }
    }

    /// a pushed `result` lands in the parent's session as one
    /// entry carrying the whole envelope. Printed line by line that buries the
    /// transcript, so it collapses to who, which attempt, which candidate, and
    /// the first line of what the child said.
    #[test]
    fn a_pushed_result_renders_as_one_compact_row_not_a_mail_dump() {
        let envelope = serde_json::json!({
            "purpose": "result",
            "from": "navtree-c1",
            "attempt_id": "navtree-c1-a2",
            "candidate": "cand-0f3a91b2",
            "output": "{\"verdict\":\"ok\"}\nsecond line\nthird line",
        })
        .to_string();
        let lines = project_transcript_lines([item("user", &envelope, 1, &[])]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].actor, "mail");
        assert_eq!(
            lines[0].text,
            "result · #c1 · a2 · #0f3a91b2 · {\"verdict\":\"ok\"}"
        );
        assert!(!lines[0].text.contains("second line"));
    }

    /// A `followup` is mail like any other and says so; it is not a user turn
    /// the operator typed.
    #[test]
    fn a_followup_renders_as_mail_with_its_purpose() {
        let envelope = serde_json::json!({
            "purpose": "followup",
            "from": "navtree-root",
            "body": "also cover the error path",
        })
        .to_string();
        let lines = project_transcript_lines([item("user", &envelope, 1, &[])]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].actor, "mail");
        assert_eq!(
            lines[0].text,
            "followup · #root · also cover the error path"
        );
    }

    /// A body the host did not author is never reshaped: ordinary JSON the
    /// child happened to print stays exactly the lines it was.
    #[test]
    fn an_ordinary_entry_is_never_reshaped_into_a_mail_row() {
        let lines = project_transcript_lines([
            item("assistant", "{\"purpose\":\"unrelated\"}", 1, &[]),
            item("user", "plain text", 1, &[]),
        ]);
        assert_eq!(
            lines
                .iter()
                .map(|line| format!("[{}] {}", line.actor, line.text))
                .collect::<Vec<_>>(),
            vec![
                "[assistant] {\"purpose\":\"unrelated\"}",
                "[user] plain text"
            ]
        );
    }

    /// The reset time and the usage counters live on the
    /// attempt, so the row reads them from there or reports nothing.
    #[test]
    fn a_blocked_attempt_projects_its_reset_and_usage() {
        let (_dir, mut tree) = tree();
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        {
            let attempt = tree.attempt_mut_for_test(&spawned.agent_id);
            attempt.status = AttemptStatus::Blocked;
            attempt.blocked = Some(crate::agent_tree::BlockedReason {
                quota: "weekly".to_owned(),
                account: "codex".to_owned(),
                resets_at: Some(1_700_000_000),
            });
            attempt.candidate_id = Some(CandidateId::from_persisted("cand-0f3a91b2".to_owned()));
            attempt.usage = crate::agent_tree::UsageRow {
                elapsed_ms: 91_000,
                turns: 3,
                tool_calls: 11,
                input_tokens: 4_200,
                output_tokens: 800,
            };
        }
        let snap = project_snapshot(&tree);
        let child = snap
            .nodes
            .iter()
            .find(|node| node.agent_id == spawned.agent_id)
            .unwrap();
        assert_eq!(
            child.attempt_status.map(AttemptStatus::as_str),
            Some("blocked")
        );
        assert_eq!(child.attempt_seq, Some(1));
        assert_eq!(child.attempts, vec![1]);
        assert_eq!(
            child.candidate_id.as_ref().map(CandidateId::as_str),
            Some("cand-0f3a91b2")
        );
        let blocked = child.blocked.as_ref().expect("the attempt is blocked");
        assert_eq!(blocked.quota, "weekly");
        assert_eq!(blocked.account, "codex");
        assert_eq!(blocked.resets_at_unix, Some(1_700_000_000));
        let usage = child.usage.expect("the attempt carries usage");
        assert_eq!(usage.elapsed_ms, 91_000);
        assert_eq!(usage.turns, 3);
        assert_eq!(usage.tool_calls, 11);
        assert_eq!(usage.input_tokens, 4_200);
        assert_eq!(usage.output_tokens, 800);
        // The root has no attempt, so it reports nothing rather than zeros.
        let root = &snap.nodes[0];
        assert!(root.usage.is_none());
        assert!(root.blocked.is_none());
        assert_eq!(root.attempt_seq, None);
        assert!(root.attempts.is_empty());
    }

    /// The row carries the template and the tier, and the
    /// workspace mode is one of `inherit|snapshot|fork`.
    #[test]
    fn the_row_carries_the_tier_and_the_workspace_status() {
        let (_dir, mut tree) = tree();
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        let snap = project_snapshot(&tree);
        let child = snap
            .nodes
            .iter()
            .find(|node| node.agent_id == spawned.agent_id)
            .unwrap();
        assert_eq!(child.template.as_deref(), Some("review"));
        assert!(
            matches!(
                child.tier.map(Tier::as_str),
                Some("read_only" | "execute" | "mutate")
            ),
            "{:?}",
            child.tier
        );
        assert!(
            ["inherit", "snapshot", "fork"].contains(&child.workspace_mode.as_str()),
            "{:?}",
            child.workspace_mode
        );
        assert!(
            child
                .workspace_status
                .map(WorkspaceStatus::as_str)
                .is_some_and(|status| {
                    plakous_host_protocol::views::PROJECTED_STATUS_LABELS.contains(&status)
                }),
            "{:?}",
            child.workspace_status
        );
    }

    #[test]
    fn snapshot_is_preorder_with_revision_and_unread() {
        let (_dir, mut tree) = tree();
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        let snap = project_snapshot(&tree);
        assert_eq!(snap.tree_id, "navtree");
        assert_eq!(snap.revision, tree.revision);
        assert!(!snap.restored_paused());
        assert_eq!(
            snap.nodes
                .iter()
                .map(|node| node.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["root", spawned.agent_id.as_str()]
        );
        assert_eq!(snap.nodes[1].parent_agent_id.as_deref(), Some("root"));
        assert_eq!(snap.nodes[1].depth, 1);
        assert!(!snap.nodes[0].has_session);
        assert_eq!(snap.nodes[0].unread_mail, 0);
    }

    #[test]
    fn projection_carries_class_role_model_turns_and_terminal_state() {
        let (_dir, mut tree) = tree();
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        let snap = project_snapshot(&tree);
        let child = snap
            .nodes
            .iter()
            .find(|node| node.agent_id == spawned.agent_id)
            .unwrap();
        assert_eq!(child.template.as_deref(), Some("review"));
        assert_eq!(
            child.description,
            tree.node(&spawned.agent_id).unwrap().description
        );
        assert!(!child.model_tuple.is_empty());
        assert_eq!(child.turns, Some(1));
        assert!(!child.finished());
        let started = tree.attempt(&spawned.agent_id).unwrap().started_at / 1_000;
        assert_eq!(child.started_at_unix, Some(started));
        assert_eq!(
            child.attempt_id.as_deref(),
            Some(spawned.attempt_id.as_str()),
            "the current attempt is named, so operator state can key on it"
        );
        assert_eq!(child.ended_at_unix, None, "a running attempt has no end");
    }

    #[test]
    fn terminal_nodes_project_terminal_and_survive_the_snapshot() {
        let (_dir, mut tree) = tree();
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        tree.close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: spawned.agent_id.clone(),
                expected_attempt_id: None,
            },
            Some("op-close"),
        )
        .unwrap();
        let snap = project_snapshot(&tree);
        let child = snap
            .nodes
            .iter()
            .find(|node| node.agent_id == spawned.agent_id)
            .expect("a closed node stays on the canvas");
        assert!(child.finished());
        let ended = tree
            .attempt(&spawned.agent_id)
            .unwrap()
            .ended_at
            .map(|ended_at| ended_at / 1_000);
        assert!(ended.is_some(), "closing ends the attempt");
        assert_eq!(child.ended_at_unix, ended);
    }

    #[test]
    fn restored_paused_is_visible() {
        let (_dir, mut tree) = tree();
        tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.restore_paused();
        let snap = project_snapshot(&tree);
        assert!(snap.restored_paused());
        assert_eq!(snap.state.as_str(), "restored_paused");
        assert_eq!(snap.nodes[0].status.as_str(), "paused");
    }

    /// A reopened singleton has nothing to resume, so the canvas never paints
    /// it paused and never offers `r`.
    #[test]
    fn a_reopened_singleton_is_not_painted_paused() {
        let (_dir, mut tree) = tree();
        tree.restore_paused();
        let snap = project_snapshot(&tree);
        assert!(!snap.restored_paused());
        assert_eq!(snap.state.as_str(), "live");
    }

    /// The canvas `Bind` line read
    /// `claude-opus-5 · low` while the header read `codex · gpt-5.6-luna · low`.
    /// A durable tree is shared across a rebind lineage, so its root record
    /// carried the successor's binding; the canvas is showing *this* session.
    #[test]
    fn the_canvas_root_bind_follows_the_attached_session() {
        let (_dir, mut tree) = tree();
        // A rebind installed a successor epoch, so the shared tree records the
        // successor's opus binding — correct for the tree, wrong for a resumed
        // source session.
        tree.begin_successor_epoch(crate::orchestration::OrchestrationSnapshot::freeze(
            crate::orchestration::RosterInput {
                workspace_idle_seconds: 600,
                workspace_budget_mib: 4096,
                child_retention_days: 7,
                child_retention_count: 128,
                root: crate::orchestration::RootBind {
                    delegation: crate::agent_tree_protocol::Delegation::default(),
                    template: None,
                    account: "claude".to_owned(),
                    tuple_id: "claude-opus-5".to_owned(),
                    effort: Some("low".to_owned()),
                },
                bind_hints: crate::orchestration::BindHints::from_names(
                    &[
                        "codex",
                        "chatgpt",
                        "grok",
                        "supergrok",
                        "xai",
                        "xai-oauth",
                        "claude",
                        "openrouter",
                    ],
                    &["openrouter"],
                ),
                ..crate::orchestration::RosterInput::unpinned("claude-opus-5")
            },
        ))
        .expect("a quiescent tree admits a successor epoch");
        let mut binding = crate::orchestration::RosterBinding::leaf("codex", "gpt-5.6-luna");
        binding.effort = Some("low".to_owned());
        binding.max_tier = Tier::ReadOnly;
        tree.pin_template("review", binding);
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        let mut snapshot = project_snapshot(&tree);
        assert_eq!(snapshot.nodes[0].model_tuple, "claude-opus-5");

        let mut session = SessionRecord::new(PathBuf::from("/work/exact"));
        session.account = Some("codex".into());
        session.model = Some("gpt-5.6-luna".into());
        session.effort = Some("low".into());
        let child_before = snapshot
            .nodes
            .iter()
            .find(|node| node.agent_id == spawned.agent_id)
            .cloned()
            .expect("child");
        apply_session_root_binding(&mut snapshot, &session);

        assert_eq!(snapshot.nodes[0].model_account, "codex");
        assert_eq!(snapshot.nodes[0].model_tuple, "gpt-5.6-luna");
        assert_eq!(snapshot.nodes[0].model_effort.as_deref(), Some("low"));
        // A child's binding is its frozen roster pin and is never rewritten.
        let child_after = snapshot
            .nodes
            .iter()
            .find(|node| node.agent_id == spawned.agent_id)
            .expect("child");
        assert_eq!(child_after.model_tuple, child_before.model_tuple);
        assert_eq!(child_after.model_account, child_before.model_account);

        // An unbound session leaves the tree's record alone rather than
        // blanking the row.
        let mut snapshot = project_snapshot(&tree);
        apply_session_root_binding(&mut snapshot, &SessionRecord::new(PathBuf::from("/w")));
        assert_eq!(snapshot.nodes[0].model_tuple, "claude-opus-5");
    }

    #[test]
    fn missing_locator_projects_has_session_false() {
        let (_dir, mut tree) = tree();
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        let snap = project_snapshot(&tree);
        let child = snap
            .nodes
            .iter()
            .find(|node| node.agent_id == spawned.agent_id)
            .unwrap();
        assert!(!child.has_session);
    }

    fn session_for(tree_id: &str, workspace: PathBuf) -> SessionRecord {
        let mut session = SessionRecord::new(workspace);
        session.id = tree_id.to_owned();
        session
    }

    #[tokio::test]
    async fn missing_hub_fails_closed_without_load() {
        let root = tempdir().unwrap();
        let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
        let paths = MemoryRows::new();
        let session = session_for(&tree_id, root.path().to_path_buf());
        let events = std::sync::Mutex::new(Vec::new());
        dispatch_topology_navigator_action(
            &paths,
            &session,
            |event| events.lock().unwrap().push(event),
            &UserAction::OpenTopologyNavigator,
        )
        .await;
        let events = events.lock().unwrap();
        assert!(matches!(
            events.as_slice(),
            [HostEvent::TopologyNavigatorUnavailable { .. }]
        ));
        assert!(crate::topology_runtime::lookup(&tree_id).is_none());
    }

    /// `ResumeTopologyTree` is an explicit intent, so a hub miss answers it
    /// like any other navigator action. It was silent only because attach used
    /// to auto-send it, which the topology contract forbids.
    #[tokio::test]
    async fn missing_hub_resume_tree_reports_unavailable_like_any_other_intent() {
        let root = tempdir().unwrap();
        let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
        let paths = MemoryRows::new();
        let session = session_for(&tree_id, root.path().to_path_buf());
        let events = std::sync::Mutex::new(Vec::new());
        dispatch_topology_navigator_action(
            &paths,
            &session,
            |event| events.lock().unwrap().push(event),
            &UserAction::ResumeTopologyTree,
        )
        .await;
        let events = events.lock().unwrap();
        assert!(matches!(
            events.as_slice(),
            [HostEvent::TopologyNavigatorUnavailable { .. }]
        ));
    }

    /// Opening the canvas over a `RestoredPaused` tree paints it paused.
    ///
    /// Spec 02 §5.7: "`RestoredPaused` never auto-resumes", `r` is "the only
    /// exit from `RestoredPaused`", §10.4 "do not auto-resume on reattach",
    /// Every intent an attach or a reattach can issue leaves the
    /// tree paused; only the explicit `ResumeTopologyTree` moves it.
    #[tokio::test]
    async fn attach_intents_paint_a_restored_paused_tree_and_never_resume_it() {
        let root = tempdir().unwrap();
        let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
        let paths = MemoryRows::new();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.clone(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        tree.spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        tree.restore_paused();
        crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
        let session = session_for(&tree_id, root.path().to_path_buf());

        for action in [
            UserAction::OpenTopologyNavigator,
            UserAction::RefreshTopologyTree,
        ] {
            let events = std::sync::Mutex::new(Vec::new());
            dispatch_topology_navigator_action(
                &paths,
                &session,
                |event| events.lock().unwrap().push(event),
                &action,
            )
            .await;
            let captured = events.lock().unwrap();
            let snapshot = captured
                .iter()
                .find_map(|event| match event {
                    HostEvent::TopologyNavigatorSnapshot(snapshot) => Some(snapshot),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{action:?} must paint the tree"));
            assert!(
                snapshot.restored_paused(),
                "{action:?} auto-resumed the tree"
            );
            assert_eq!(snapshot.state.as_str(), "restored_paused", "{action:?}");
            assert_eq!(
                crate::topology_runtime::lookup(&tree_id)
                    .unwrap()
                    .tree_handle_for_test()
                    .lock()
                    .unwrap()
                    .state,
                TreeAdmissionState::RestoredPaused,
                "{action:?} must leave the persisted epoch paused"
            );
        }
        crate::topology_runtime::force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn hub_open_emits_snapshot() {
        let root = tempdir().unwrap();
        let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
        let paths = MemoryRows::new();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.clone(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        tree.spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        let revision = tree.revision;
        crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
        let session = session_for(&tree_id, root.path().to_path_buf());
        let events = std::sync::Mutex::new(Vec::new());
        dispatch_topology_navigator_action(
            &paths,
            &session,
            |event| events.lock().unwrap().push(event),
            &UserAction::OpenTopologyNavigator,
        )
        .await;
        {
            let captured = events.lock().unwrap();
            let HostEvent::TopologyNavigatorSnapshot(snap) = &captured[0] else {
                panic!("expected snapshot, got {:?}", captured[0]);
            };
            assert_eq!(snap.tree_id, tree_id);
            assert_eq!(snap.revision, revision);
            assert!(!snap.restored_paused());
        }
        crate::topology_runtime::force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn resume_and_interrupt_resnapshot() {
        let root = tempdir().unwrap();
        let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
        let paths = MemoryRows::new();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.clone(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn"))
            .unwrap();
        tree.restore_paused();
        crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
        let session = session_for(&tree_id, root.path().to_path_buf());
        let events = std::sync::Mutex::new(Vec::new());
        dispatch_topology_navigator_action(
            &paths,
            &session,
            |event| events.lock().unwrap().push(event),
            &UserAction::ResumeTopologyTree,
        )
        .await;
        {
            let captured = events.lock().unwrap();
            let snap = captured
                .iter()
                .find_map(|event| match event {
                    HostEvent::TopologyNavigatorSnapshot(snap) => Some(snap),
                    _ => None,
                })
                .unwrap();
            assert!(!snap.restored_paused());
            assert_eq!(snap.state.as_str(), "live");
            assert!(
                !captured
                    .iter()
                    .any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(_)))),
                "resuming a paused tree is not a no-op: {captured:?}"
            );
        }
        events.lock().unwrap().clear();
        // A second resume finds the tree live and says so instead of
        // returning silently.
        dispatch_topology_navigator_action(
            &paths,
            &session,
            |event| events.lock().unwrap().push(event),
            &UserAction::ResumeTopologyTree,
        )
        .await;
        {
            let captured = events.lock().unwrap();
            assert!(
                captured.iter().any(|event| matches!(
                    event,
                    HostEvent::Row(row) if row.text() == Some("agent tree is not paused; nothing to resume")
                )),
                "{captured:?}"
            );
            assert!(
                captured
                    .iter()
                    .any(|event| matches!(event, HostEvent::TopologyNavigatorSnapshot(_)))
            );
        }
        events.lock().unwrap().clear();
        let revision = crate::topology_runtime::lookup(&tree_id)
            .unwrap()
            .tree_handle_for_test()
            .lock()
            .unwrap()
            .revision;
        dispatch_topology_navigator_action(
            &paths,
            &session,
            |event| events.lock().unwrap().push(event),
            &UserAction::InterruptTopologyNode {
                agent_id: spawned.agent_id.clone(),
            },
        )
        .await;
        {
            let captured = events.lock().unwrap();
            assert!(
                captured
                    .iter()
                    .any(|event| matches!(event, HostEvent::TopologyNavigatorSnapshot(_))),
                "interrupt must resnapshot, got {captured:?}"
            );
        }
        let _ = revision;
        crate::topology_runtime::force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn refresh_re_emits_the_current_revision_even_when_it_did_not_advance() {
        let root = tempdir().unwrap();
        let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
        let paths = MemoryRows::new();
        let tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.clone(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let revision = tree.revision;
        crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
        let session = session_for(&tree_id, root.path().to_path_buf());
        let events = std::sync::Mutex::new(Vec::new());
        for _ in 0..2 {
            dispatch_topology_navigator_action(
                &paths,
                &session,
                |event| events.lock().unwrap().push(event),
                &UserAction::RefreshTopologyTree,
            )
            .await;
        }
        {
            let captured = events.lock().unwrap();
            let snaps: Vec<&TreeView> = captured
                .iter()
                .filter_map(|event| match event {
                    HostEvent::TopologyNavigatorSnapshot(snap) => Some(snap),
                    _ => None,
                })
                .collect();
            assert_eq!(snaps.len(), 2, "refresh always answers");
            assert!(snaps.iter().all(|snap| snap.revision == revision));
            assert!(snaps.iter().all(|snap| snap.solicited));
        }
        crate::topology_runtime::force_release(&tree_id).await;
    }
    struct RetiringChild {
        entered: Arc<tokio::sync::Notify>,
        finish: Arc<tokio::sync::Notify>,
    }
    impl crate::topology_runtime::LiveChildHandle for RetiringChild {
        fn agent(&self) -> Option<plakous::agent::Agent> {
            None
        }
        fn turn_id(&self) -> plakous::model::TurnId {
            plakous::model::TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap()
        }
        fn turn_control(&self) -> plakous::agent::TurnControl {
            panic!("fixture")
        }
        fn follow_up(
            &self,
            _: plakous::agent::CommandId,
            _: String,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            Box::pin(async { Err("fixture".into()) })
        }
        fn restart(
            &mut self,
            _: crate::agent_tree_tools::TreeDriver,
            _: plakous::agent::CommandId,
            _: String,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
        > {
            Box::pin(async { Err("fixture".into()) })
        }
        fn release(
            self: Box<Self>,
            driver: crate::agent_tree_tools::TreeDriver,
        ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>>
        {
            self.cancel_and_release(driver)
        }
        fn cancel_and_release(
            self: Box<Self>,
            _driver: crate::agent_tree_tools::TreeDriver,
        ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>>
        {
            Box::pin(async move {
                self.entered.notify_one();
                self.finish.notified().await;
                crate::topology_runtime::LiveRelease {
                    feed: None,
                    locator: None,
                    failure: Some("release refused".into()),
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn navigator_joiner_expiry_does_not_refault_a_settled_release() {
        use std::future::Future;
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll};
        let root = tempdir().unwrap();
        let mut tree = crate::topology_runtime::test_support::bootstrap(
            "navigator-joiner-race",
            root.path().to_path_buf(),
        );
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap()
            .agent_id;
        tree.set_active_turn(&child, true).unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            tree,
            crate::topology_runtime::test_support::memory_store(),
            Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    child.clone(),
                    Box::new(RetiringChild {
                        entered: entered.clone(),
                        finish: finish.clone()
                    })
                )
                .await
                .is_ok()
        );
        let session = session_for("navigator-joiner-race", root.path().to_path_buf());
        let events = Arc::new(Mutex::new(Vec::new()));
        let owner = runtime.clone();
        let id = child.clone();
        let notes = events.clone();
        let pending: std::pin::Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
            close_node(
                &session,
                &owner,
                |event| notes.lock().unwrap().push(event),
                &id,
            )
            .await;
        });
        let pending = Arc::new(Mutex::new(Some(pending)));
        let close = pending.clone();
        let weak = Arc::downgrade(&runtime);
        let id = child.clone();
        runtime
            .set_retirement_recovery(Arc::new(move || {
                let owner = weak.upgrade().unwrap();
                // The deadline's early recovery request cannot reopen a live owner.
                if owner.live.try_lock().unwrap().contains_key(&id) {
                    return;
                }
                owner
                    .mutate_then_emit(false, |tree| {
                        let attempt = tree.attempt(&id).unwrap().attempt_id.clone();
                        tree.recover_host_decision(
                            &id,
                            &attempt,
                            crate::agent_tree::RecoveryResolution::RecoveredWithoutTurn {
                                message: "late release is quiescent".into(),
                            },
                        )
                        .map_err(|error| error.message)?;
                        Ok(())
                    })
                    .unwrap();
                let pending = close.lock().unwrap().take();
                if let Some(mut close) = pending {
                    let mut cx = Context::from_waker(std::task::Waker::noop());
                    assert!(matches!(close.as_mut().poll(&mut cx), Poll::Ready(())));
                }
            }))
            .await;
        {
            let mut close = pending.lock().unwrap();
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(close.as_mut().unwrap().as_mut().poll(&mut cx).is_pending());
        }
        entered.notified().await;
        tokio::time::advance(
            plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT
                + plakous_host_protocol::limits::HOST_SESSION_RELEASE_TIMEOUT
                + std::time::Duration::from_secs(1),
        )
        .await;
        tokio::task::yield_now().await;
        finish.notify_one();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(pending.lock().unwrap().is_none());
        runtime
            .read(|tree| {
                assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
                assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Closed);
                assert_eq!(
                    tree.recovery_need(&child).unwrap(),
                    crate::agent_tree::RecoveryNeed::NotRequired
                );
                assert!(tree.pending_result_mail("root").is_empty());
            })
            .unwrap();
        assert!(events.lock().unwrap().iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: message, .. })) if message.contains("owner retained"))));
    }

    #[tokio::test]
    async fn interrupt_then_overlapping_close_and_stop_all_join_retirement_without_sealing() {
        use crate::occupancy_vcs::fake::{FakeOp, FakeVcs};
        use std::future::Future;
        use std::sync::Arc;
        let root = tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "close-retiring",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding =
            crate::orchestration::RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let child = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(
                    "implement",
                    Tier::Mutate,
                    "implementer",
                    "edit the parser",
                ),
                None,
            )
            .unwrap()
            .agent_id;
        tree.set_active_turn(&child, true).unwrap();
        tree.interrupt_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: child.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            tree,
            crate::topology_runtime::test_support::memory_store(),
            vcs.clone(),
            None,
        ));
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        assert!(
            runtime
                .try_occupy_live(
                    child.clone(),
                    Box::new(RetiringChild {
                        entered: entered.clone(),
                        finish: finish.clone(),
                    })
                )
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let id = child.clone();
        let cancel = tokio::spawn(async move { owner.cancel_live(&id).await });
        entered.notified().await;
        let session = session_for("close-retiring", root.path().to_path_buf());
        let events = std::sync::Mutex::new(Vec::new());
        let send = |event| events.lock().unwrap().push(event);
        let mut close = Box::pin(close_node(&session, &runtime, send, &child));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx).is_pending()))
                .await
        );
        let mut repeated = Box::pin(close_node(&session, &runtime, send, &child));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                repeated.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        let mut stop = Box::pin(stop_all(&session, &runtime, send));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(stop.as_mut().poll(cx).is_pending()))
                .await
        );
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
        assert!(
            runtime
                .read(|tree| tree.node(&child).unwrap().active_turn)
                .unwrap()
        );
        finish.notify_one();
        assert_eq!(
            cancel.await.unwrap().unwrap_err().to_string(),
            "release refused"
        );
        close.await;
        repeated.await;
        stop.await;
        runtime
            .read(|tree| {
                assert_eq!(
                    tree.node(&child).unwrap().status,
                    NodeStatus::RecoveryRequired
                );
                assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Closed);
                assert!(matches!(
                    tree.recovery_need(&child).unwrap(),
                    crate::agent_tree::RecoveryNeed::Session { .. }
                ));
                assert_eq!(tree.fault_reason(&child), Some("release refused"));
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
        assert!(events.lock().unwrap().iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: message, .. })) if message.contains("release refused"))));
    }
}
