//! Host projection of the inert TUI tree navigator.
//!
//! The TUI never opens an Agent or store. This module snapshots `ProductAgentTree`
//! in deterministic preorder. Missing hub fails closed.

use crate::surface_protocol::{ContentEvent, UserAction};
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
    send: impl Fn(ContentEvent<P>),
    reason: &str,
) {
    send(ContentEvent::TopologyNavigatorUnavailable {
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
    events: &crate::runtime::Sender<P>,
    action: &UserAction<P>,
) {
    let send = |event| {
        let _ = events.send(event);
    };
    let tree_id = tree_id_for_session(rows, session);
    let Some(runtime) = crate::topology_runtime::lookup(&tree_id) else {
        match action {
            UserAction::OpenAgentTranscript { agent_id } => {
                events.replace_transcript(PresentationSnapshot {
                    scope: TranscriptScope::Agent(agent_id.clone()),
                    items: vec![KeyedItem {
                        key: RowKey::TRUNCATION,
                        row: TranscriptRow::info(
                            NoticeScope::Tree,
                            "no agent tree is attached to this session",
                        ),
                    }],
                    ..PresentationSnapshot::default()
                });
                return;
            }
            UserAction::CloseAgentTranscript => return,
            _ => {}
        }
        // A navigator intent is explicit operator input — never part of an
        // attach sequence — so a hub miss is reported, not swallowed.
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
            emit_snapshot(&runtime, session, send, true);
        }
        UserAction::InterruptTopologyNode { agent_id } => {
            interrupt_node(session, &runtime, &send, agent_id).await;
        }
        UserAction::CloseTopologyNode { agent_id } => {
            close_node(session, &runtime, &send, agent_id).await;
        }
        UserAction::StopAllTopology => stop_all(session, &runtime, &send).await,
        UserAction::RecoverTopologyNode {
            tree_id,
            revision,
            agent_id,
            expected_attempt_id,
        } => {
            if tree_id != &tree_id_for_session(rows, session) {
                send(ContentEvent::Row(TranscriptRow::error(
                    NoticeScope::Tree,
                    "this session has a different tree",
                )));
                return;
            }
            spawn_operator_child_recover(
                runtime,
                session,
                events,
                *revision,
                agent_id.clone(),
                expected_attempt_id.clone(),
                send,
            );
        }
        _ => {}
    }
}

/// Starts one child recover on the host executor and returns. The hub keeps
/// the task, and shutdown aborts it. The tree mutex is not held while the
/// lease wait runs.
fn spawn_operator_child_recover<P: plakous_host_protocol::ProtocolExtension>(
    runtime: std::sync::Arc<ProductTopologyRuntime>,
    session: &SessionRecord,
    events: &crate::runtime::Sender<P>,
    revision: u64,
    agent_id: String,
    expected_attempt_id: String,
    send: impl Fn(ContentEvent<P>) + Copy,
) {
    if runtime.is_closed() {
        send(ContentEvent::Row(TranscriptRow::error(
            NoticeScope::Tree,
            "agent tree navigator unavailable: hub is shutting down",
        )));
        return;
    }
    let task_runtime = std::sync::Arc::clone(&runtime);
    let task_session = session.clone();
    let task_events = events.clone();
    let task = runtime.driver().tasks().spawn(async move {
        let send = |event| {
            let _ = task_events.send(event);
        };
        match crate::agent::operator_child_recover(
            &task_runtime,
            revision,
            &agent_id,
            &expected_attempt_id,
        )
        .await
        {
            Ok(output) => {
                let note = if output.recovered {
                    format!("recovered {}", short_agent(&agent_id))
                } else {
                    format!("{} was not waiting on recovery", short_agent(&agent_id))
                };
                send(ContentEvent::Row(TranscriptRow::info(
                    NoticeScope::Tree,
                    note,
                )));
            }
            Err(error) => {
                send(ContentEvent::Row(TranscriptRow::error(
                    NoticeScope::Tree,
                    error,
                )));
            }
        }
        emit_snapshot(&task_runtime, &task_session, send, true);
    });
    if !runtime.track_recovery(task) {
        send(ContentEvent::Row(TranscriptRow::error(
            NoticeScope::Tree,
            "agent tree navigator unavailable: hub is shutting down",
        )));
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
    send: impl Fn(ContentEvent<P>),
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
async fn mutate_tree<R: Send + 'static, P: plakous_host_protocol::ProtocolExtension>(
    runtime: &ProductTopologyRuntime,
    send: impl Fn(ContentEvent<P>) + Copy,
    what: &str,
    work: impl FnOnce(&mut ProductAgentTree) -> Result<R, String> + Send + 'static,
) -> Option<R> {
    match runtime.mutate_then_emit(false, work).await {
        Ok(result) => Some(result),
        Err(error) => {
            send(ContentEvent::Row(TranscriptRow::error(
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
    send: impl Fn(ContentEvent<P>) + Copy,
    solicited: bool,
) {
    let Some(mut snapshot) = read_tree(runtime, send, project_snapshot) else {
        return;
    };
    apply_session_root_binding(&mut snapshot, session);
    snapshot.solicited = solicited;
    send(ContentEvent::TopologyNavigatorSnapshot(snapshot));
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
    send: impl Fn(ContentEvent<P>) + Copy,
    agent_id: &str,
) {
    let operation_id = uuid::Uuid::new_v4().to_string();
    let agent = agent_id.to_owned();
    if mutate_tree(runtime, send, "interrupt failed", move |tree| {
        let root = tree.root_agent_id.clone();
        tree.interrupt_agent(
            &root,
            &AttemptScopedRequest {
                agent_id: agent,
                expected_attempt_id: None,
            },
            Some(&operation_id),
        )
        .map(|_| ())
        .map_err(|error| error.message)
    })
    .await
    .is_none()
    {
        return;
    }
    signal_retirement(runtime, &[agent_id.to_owned()]).await;
    emit_snapshot(runtime, session, send, false);
}

async fn close_node<P: plakous_host_protocol::ProtocolExtension>(
    session: &SessionRecord,
    runtime: &ProductTopologyRuntime,
    send: impl Fn(ContentEvent<P>) + Copy,
    agent_id: &str,
) {
    let operation_id = uuid::Uuid::new_v4().to_string();
    let agent = agent_id.to_owned();
    let Some(closed) = mutate_tree(runtime, send, "close failed", move |tree| {
        let root = tree.root_agent_id.clone();
        let closed = tree
            .close_agent(
                &root,
                &AttemptScopedRequest {
                    agent_id: agent,
                    expected_attempt_id: None,
                },
                Some(&operation_id),
            )
            .map_err(|error| error.message)?;
        Ok(closed)
    })
    .await
    else {
        return;
    };
    let closed_ids: Vec<String> = closed.into_iter().map(|(id, _)| id).collect();
    signal_retirement(runtime, &closed_ids).await;
    emit_snapshot(runtime, session, send, false);
}

async fn stop_all<P: plakous_host_protocol::ProtocolExtension>(
    session: &SessionRecord,
    runtime: &ProductTopologyRuntime,
    send: impl Fn(ContentEvent<P>) + Copy,
) {
    let operation_id = uuid::Uuid::new_v4().to_string();
    let Some(closed) = mutate_tree(runtime, send, "stop-all failed", move |tree| {
        let root = tree.root_agent_id.clone();
        let closed = tree
            .stop_all(&root, Some(&operation_id))
            .map_err(|error| error.message)?;
        Ok(closed)
    })
    .await
    else {
        return;
    };
    let closed_ids: Vec<String> = closed.into_iter().map(|(id, _)| id).collect();
    signal_retirement(runtime, &closed_ids).await;
    emit_snapshot(runtime, session, send, false);
}

async fn signal_retirement(runtime: &ProductTopologyRuntime, ids: &[String]) {
    {
        let live = runtime.live.lock().await;
        let detached: Vec<String> = ids
            .iter()
            .filter(|id| !live.contains_key(id))
            .cloned()
            .collect();
        let observer = runtime.observer();
        let _ = runtime
            .driver()
            .mutate(move |tree| {
                for id in &detached {
                    if let Some(status) = tree.node(id).ok().and_then(|node| {
                        crate::agent_tree::NodeDetach::from_node_status(node.status)
                    }) {
                        tree.try_emit_tree_node_detached(observer.as_ref(), id, status);
                    }
                }
            })
            .await;
    }
    let join = runtime.begin_cancel_live_many(ids).await;
    runtime.driver().tasks().spawn(async move {
        let report = join.await;
        if let Some(error) = report.first_failure {
            // The release owner persists and requests recovery for its fault.
            tracing::error!(target: "plak.topology", error, "navigator retirement failed");
        }
    });
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

#[cfg(test)]
mod tests;
