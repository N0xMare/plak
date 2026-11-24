use crate::carry::{TraceHistory, redact_payload};
use crate::home::paths::PlakPaths;
use crate::store::ProductStore;
use crate::store::directory::TraceRoot;
use crate::store::sessions::SessionRecord;
use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionLocator};
use plakous_host_protocol::topology::{MessagePriority, MessagePurpose, NodeStatus};
use plakous_hosted::agent_tree::{
    AttemptRecord, MailDisposition, MailDurableState, MailRecord, NodeRecord, ProductAgentTree,
};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_recursive_subagents_store::{SqliteTreeStore, TreeInspection};
use serde::Serialize;
use std::io;
use std::path::PathBuf;
use time::OffsetDateTime;

const FORMAT: &str = "plak-child-trace";
const VERSION: u32 = 1;
const EXCLUSIONS: &[&str] = &[
    "compacted_or_clipped_material",
    "hidden_reasoning_and_provider_replay",
    "mail_bodies_outside_canonical_history",
    "uncommitted_events_raw_process_captures_and_workspaces",
    "stored_credentials_account_authority_and_named_state",
    "per_request_route_usage_and_complete_event_timeline",
];

#[derive(Eq, PartialEq, Serialize)]
struct TraceSource {
    child_id: String,
    child_agent_id: String,
    tree_id: String,
    root_session_id: String,
    root_agent_id: String,
    parent_agent_id: String,
    attempt_id: String,
    head: String,
    runtime_session_id: String,
    branch_id: String,
    checkpoint_id: String,
    revision: u64,
    settled_at_unix_ms: u64,
}

impl TraceSource {
    fn from_child(root: &str, tree: &ProductAgentTree, child: &Child<'_>, revision: u64) -> Self {
        Self {
            child_id: format!("{root}.{}", child.node.agent_id),
            child_agent_id: child.node.agent_id.clone(),
            tree_id: tree.tree_id.clone(),
            root_session_id: root.to_owned(),
            root_agent_id: tree.root_agent_id.clone(),
            parent_agent_id: child.parent.to_owned(),
            attempt_id: child.attempt.attempt_id.as_str().to_owned(),
            head: child.locator.to_string(),
            runtime_session_id: child.locator.session_id().to_string(),
            branch_id: child.locator.branch_id().to_string(),
            checkpoint_id: child.locator.checkpoint_id().to_string(),
            revision,
            settled_at_unix_ms: child.ended_at,
        }
    }
}

#[derive(Serialize)]
struct Trace {
    format: &'static str,
    version: u32,
    state_key: String,
    producer_binary_id: Option<&'static str>,
    captured_at_unix_ms: u64,
    source: TraceSource,
    model: String,
    route: String,
    effort: Option<String>,
    attempt: Attempt,
    mail: Vec<Mail>,
    history: TraceHistory,
    unavailable: &'static [&'static str],
}

#[derive(Serialize)]
struct Attempt {
    status: &'static str,
    retry_of: Option<String>,
    started_at_unix_ms: u64,
    failure_kind: Option<String>,
    result: Option<SubmittedResult>,
}

impl From<&AttemptRecord> for Attempt {
    fn from(attempt: &AttemptRecord) -> Self {
        Self {
            status: attempt.status.as_str(),
            retry_of: attempt.retry_of.as_ref().map(ToString::to_string),
            started_at_unix_ms: attempt.started_at,
            failure_kind: attempt
                .failure
                .as_ref()
                .map(|failure| redact_payload(&failure.kind)),
            result: attempt.output_json.as_deref().map(|output| {
                let output_json = redact_payload(output);
                SubmittedResult {
                    source_sha256: plakous::model::sha256_hex(&[output_json.as_bytes()]),
                    output_json,
                }
            }),
        }
    }
}

#[derive(Serialize)]
struct SubmittedResult {
    source_sha256: String,
    output_json: String,
}

#[derive(Serialize)]
struct Mail {
    message_id: String,
    from: String,
    to: String,
    purpose: MessagePurpose,
    priority: MessagePriority,
    in_reply_to: Option<String>,
    disposition: MailDisposition,
    durable: MailDurableState,
    result_attempt_id: Option<String>,
}

impl From<&MailRecord> for Mail {
    fn from(mail: &MailRecord) -> Self {
        let result_attempt_id = if mail.purpose == MessagePurpose::Result {
            serde_json::from_str::<serde_json::Value>(&mail.body)
                .ok()
                .and_then(|body| body["attempt_id"].as_str().map(str::to_owned))
        } else {
            None
        };
        Self {
            message_id: mail.message_id.clone(),
            from: mail.from.clone(),
            to: mail.to.clone(),
            purpose: mail.purpose,
            priority: mail.priority,
            in_reply_to: mail.in_reply_to.clone(),
            disposition: mail.disposition,
            durable: mail.durable,
            result_attempt_id,
        }
    }
}

struct Child<'a> {
    node: &'a NodeRecord,
    parent: &'a str,
    attempt: &'a AttemptRecord,
    ended_at: u64,
    locator: SessionLocator,
}

fn settled_child<'a>(tree: &'a ProductAgentTree, id: &str) -> CliResult<Child<'a>> {
    let node = tree
        .nodes()
        .get(id)
        .filter(|node| node.parent_agent_id.is_some())
        .ok_or_else(|| CliFailure::new("unknown child in session tree"))?;
    if !matches!(
        node.status,
        NodeStatus::Completed | NodeStatus::Failed | NodeStatus::Closed | NodeStatus::Interrupted
    ) || node.active_turn
        || tree.has_turn_owner(id)
        || tree.has_turn_waiter(id)
        || tree.is_parked(id)
        || tree.recovery_owed(id)
        || tree.terminal_recovery_refusal(id).is_some()
    {
        return Err(CliFailure::new(format!(
            "child {id} is not settled ({:?})",
            node.status
        )));
    }
    if tree.retired_children().contains_key(id) || tree.conversation_collected(id) {
        return Err(CliFailure::new("child conversation is retired or retiring"));
    }
    if tree.unread_mail(id).next().is_some()
        || tree
            .commands()
            .values()
            .any(|cmd| cmd.recipient == id && !cmd.withdrawn && cmd.fact.is_none())
    {
        return Err(CliFailure::new("child has pending input"));
    }
    let (attempt, ended_at) = tree
        .attempt(id)
        .filter(|attempt| attempt.status.is_stopped())
        .and_then(|attempt| Some((attempt, attempt.ended_at?)))
        .ok_or_else(|| CliFailure::new("child has no settled attempt"))?;
    let parent = node
        .parent_agent_id
        .as_deref()
        .ok_or_else(|| CliFailure::new("child has no parent"))?;
    Ok(Child {
        node,
        parent,
        attempt,
        ended_at,
        locator: super::read_only::child_locator(node)?,
    })
}

fn selected_tree(paths: &PlakPaths, root: &str) -> CliResult<TreeInspection> {
    let record: SessionRecord = ProductStore::inspect_session_read_only(paths, root)?;
    if record.node.agent_id != record.node.tree_id {
        return Err(CliFailure::new(
            "trace address must name a root product session",
        ));
    }
    crate::topology_product::inspect_session_tree(paths, &record).map(|(_, tree)| tree)
}

fn capture(paths: &PlakPaths, root: &str, child: &str) -> CliResult<Trace> {
    let TreeInspection::Tree(tree) = selected_tree(paths, root)? else {
        return Err(CliFailure::new("unknown child: no persisted tree"));
    };
    let selected = settled_child(tree.rows(), child)?;
    let (revision, binding, segments) =
        AgentSessions::inspect_settled_history_sqlite(paths.runtime_database(), selected.locator)
            .map_err(|error| CliFailure::new(format!("child trace read failed: {error}")))?;
    let source = TraceSource::from_child(root, tree.rows(), &selected, revision);
    let attempt = Attempt::from(selected.attempt);
    let mail = tree
        .rows()
        .messages()
        .values()
        .filter(|mail| mail.from == child || mail.to == child)
        .map(Mail::from)
        .collect();
    let history = TraceHistory::encode(&segments)
        .map_err(|error| CliFailure::new(format!("child trace encoding failed: {error}")))?;
    recheck_capture(paths, &source, &selected)?;
    let binary_id = crate::build_identity::binary_id();
    Ok(Trace {
        format: FORMAT,
        version: VERSION,
        state_key: crate::home::state::state_key(),
        producer_binary_id: (binary_id != "unavailable").then_some(binary_id),
        captured_at_unix_ms: plakous::model::now_unix_millis(),
        source,
        model: redact_payload(binding.model().as_str()),
        route: redact_payload(binding.route().as_str()),
        effort: selected.node.effort.as_deref().map(redact_payload),
        attempt,
        mail,
        history,
        unavailable: EXCLUSIONS,
    })
}

fn recheck_capture(paths: &PlakPaths, source: &TraceSource, selected: &Child<'_>) -> CliResult<()> {
    let root = &source.root_session_id;
    let TreeInspection::Tree(current_tree) = selected_tree(paths, root)? else {
        return Err(CliFailure::new("child tree changed during trace read"));
    };
    let current = settled_child(current_tree.rows(), &source.child_agent_id)?;
    let current_revision =
        AgentSessions::inspect_settled_revision_sqlite(paths.runtime_database(), current.locator)
            .map_err(|error| CliFailure::new(format!("child trace recheck failed: {error}")))?;
    if *source != TraceSource::from_child(root, current_tree.rows(), &current, current_revision)
        || selected.attempt != current.attempt
        || selected.node.effort != current.node.effort
    {
        return Err(CliFailure::new("child changed during trace read; retry"));
    }
    Ok(())
}

pub(crate) fn write_trace(
    paths: &PlakPaths,
    address: &str,
    out: Option<PathBuf>,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let (root, child) = super::parse_read_address(paths, address)?;
    let child = child.ok_or_else(|| CliFailure::new("child session address required"))?;
    let document = capture(paths, &root, child)?;
    let path = match out {
        Some(path) => path,
        None => crate::carry::private_dir(&paths.home().join("traces"))
            .map_err(CliFailure::new)?
            .join(format!("{root}.{child}.{}.json", uuid::Uuid::new_v4())),
    };
    crate::carry::write_private_json(&path, &document).map_err(CliFailure::new)?;
    writeln!(stdout, "{}", path.display()).map_err(|_| CliFailure::new("stdout failed"))
}

pub(crate) fn list_traces(
    paths: &PlakPaths,
    since: Option<OffsetDateTime>,
    mut stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    let roots = ProductStore::trace_roots(paths)?;
    let allow_absent = roots.is_empty();
    if !SqliteTreeStore::inspect_store(&paths.subagents_database())
        .map_err(|error| CliFailure::new(format!("trace inventory tree store: {error}")))?
        && !allow_absent
    {
        return Err(missing_tree_store());
    }
    inspect_runtime_inventory(paths, allow_absent)?;
    let mut revisions = Vec::new();
    for root in roots {
        let root = match root {
            TraceRoot::Unique(root) => root,
            TraceRoot::Ambiguous(tree) => {
                inventory_skip(stderr, &tree, InventorySkip::AmbiguousRoot)?;
                continue;
            }
        };
        let tree = match selected_tree(paths, &root)? {
            TreeInspection::Tree(tree) => tree,
            TreeInspection::TreeAbsent => continue,
            TreeInspection::StoreAbsent => return Err(missing_tree_store()),
        };
        for id in tree.rows().nodes().keys() {
            let Ok(child) = settled_child(tree.rows(), id) else {
                continue;
            };
            if since.is_some_and(|since| {
                i128::from(child.ended_at) * 1_000_000 < since.unix_timestamp_nanos()
            }) {
                continue;
            }
            let revision = match AgentSessions::inspect_settled_revision_sqlite(
                paths.runtime_database(),
                child.locator,
            ) {
                Ok(revision) => revision,
                Err(error) => {
                    skip_runtime_entry(paths, stderr, &format!("{root}.{id}"), &error)?;
                    continue;
                }
            };
            revisions.push(TraceSource::from_child(
                &root,
                tree.rows(),
                &child,
                revision,
            ));
        }
    }
    revisions.sort_unstable_by(|a, b| {
        (a.settled_at_unix_ms, &a.child_id).cmp(&(b.settled_at_unix_ms, &b.child_id))
    });
    serde_json::to_writer(&mut stdout, &revisions)
        .and_then(|()| writeln!(stdout).map_err(serde_json::Error::io))
        .map_err(|_| CliFailure::new("stdout failed"))
}

fn missing_tree_store() -> CliFailure {
    CliFailure::new("trace inventory tree store is missing for a home with saved roots")
}

fn inspect_runtime_inventory(paths: &PlakPaths, allow_absent: bool) -> CliResult<()> {
    // With no branch selectors this admits just the database snapshot.
    match AgentSessions::inspect_branch_revisions_sqlite(paths.runtime_database(), &[]) {
        Ok(_) => Ok(()),
        Err(error) if allow_absent && error.kind() == AgentSessionsErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliFailure::new(format!(
            "trace inventory runtime store: {error}"
        ))),
    }
}

fn skip_runtime_entry(
    paths: &PlakPaths,
    stderr: &mut dyn io::Write,
    id: &str,
    error: &plakous::sessions::AgentSessionsError,
) -> CliResult<()> {
    let kind = error.kind();
    match kind {
        // The store itself can disappear after admission. Only a missing
        // session in a still-admitted home is an entry-scoped refusal.
        AgentSessionsErrorKind::NotFound => inspect_runtime_inventory(paths, false)?,
        AgentSessionsErrorKind::StaleLocator
        | AgentSessionsErrorKind::InvalidInput
        | AgentSessionsErrorKind::UnsettledProcesses => {}
        _ => {
            return Err(CliFailure::new(format!(
                "trace inventory runtime read failed for {id:?} ({kind:?}): {error}"
            )));
        }
    }
    inventory_skip(stderr, id, InventorySkip::RuntimeRead(kind))
}

enum InventorySkip {
    AmbiguousRoot,
    RuntimeRead(AgentSessionsErrorKind),
}

impl std::fmt::Display for InventorySkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AmbiguousRoot => f.write_str("ambiguous_root"),
            Self::RuntimeRead(kind) => write!(f, "runtime_read kind={kind:?}"),
        }
    }
}

fn inventory_skip(stderr: &mut dyn io::Write, id: &str, reason: InventorySkip) -> CliResult<()> {
    writeln!(stderr, "trace inventory skipped {id:?}: reason={reason}")
        .map_err(|_| CliFailure::new("stderr failed"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
