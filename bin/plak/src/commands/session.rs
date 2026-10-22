//! Product session lookup, mutation confirmation and attached one-shot actions.

use std::env;

use plakous_hosted::sessions::{NoticePlacement, NoticeTone, TranscriptRow};
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::host_config::{attached_target_accepts_effort, build_host_config};
use super::support::{HOST_WORKER_JOIN_TIMEOUT, join_host_worker_bounded};
use crate::presentation::tui_presentation_bundle;
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous::sessions::{
    AgentSessions, AgentSessionsErrorKind, RecoveryRequiredReport, SessionLocator,
};
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use plakous_hosted::runtime as product_host;
use plakous_recursive_subagents_store::TreeInspection;

mod read_only;
pub(crate) use read_only::{log_child_read_only, parse_read_address, show_selected_child};

/// Prints only canonical recovery identities and obligations. An out-of-date
/// product locator is displayed as recorded, then the exact current head is
/// inspected once; this never rewrites the product record or opens a runtime.
pub(crate) fn show_recovery_status(
    paths: &crate::home::paths::PlakPaths,
    record: &product_sessions::SessionRecord,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    writeln!(stdout, "\nCanonical recovery (root session)")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let Some(recorded) = record.plakous_locator else {
        return writeln!(stdout, "  Recorded head  unavailable")
            .map_err(|_| CliFailure::new("stdout failed"));
    };
    writeln!(stdout, "  Recorded head  {recorded}")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let (current, recovery) = inspect_locator(paths, recorded)?;
    if current != recorded {
        writeln!(
            stdout,
            "  Current head   {current} (recorded head is stale; unchanged)"
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    match recovery {
        None => writeln!(stdout, "  Status         quiescent at {current}")
            .map_err(|_| CliFailure::new("stdout failed")),
        Some(report) => write_recovery_report(paths, stdout, &report),
    }
}

fn inspect_locator(
    paths: &crate::home::paths::PlakPaths,
    recorded: SessionLocator,
) -> CliResult<(SessionLocator, Option<RecoveryRequiredReport>)> {
    Ok(
        match AgentSessions::inspect_recovery_sqlite(paths.runtime_database(), recorded) {
            Ok(recovery) => (recorded, recovery),
            Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                let current = error.current_locator().ok_or_else(|| {
                    CliFailure::new("stale canonical locator did not name the current head")
                })?;
                let recovery =
                    AgentSessions::inspect_recovery_sqlite(paths.runtime_database(), current)
                        .map_err(|error| {
                            CliFailure::new(format!(
                                "current canonical head changed or could not be inspected: {error}"
                            ))
                        })?;
                (current, recovery)
            }
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "canonical recovery inspection failed: {error}"
                )));
            }
        },
    )
}

fn write_recovery_report(
    paths: &crate::home::paths::PlakPaths,
    stdout: &mut dyn io::Write,
    report: &RecoveryRequiredReport,
) -> CliResult<()> {
    let status = working_status(paths, report.locator())?;
    writeln!(stdout, "  Status         {status}")
        .and_then(|()| writeln!(stdout, "  Working head   {}", report.locator()))
        .and_then(|()| writeln!(stdout, "  Phase          {:?}", report.phase()))
        .map_err(|_| CliFailure::new("stdout failed"))?;
    for obligation in report.obligations() {
        writeln!(
            stdout,
            "  Obligation     operation={} action={:?} request_identity={}",
            obligation.operation_id(),
            obligation.action(),
            obligation.request_identity()
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(())
}

fn working_status(
    paths: &crate::home::paths::PlakPaths,
    locator: SessionLocator,
) -> CliResult<&'static str> {
    let lock = paths
        .state_dir()
        .join("runtime.sqlite3-holders")
        .join(format!(
            "{}.{}.lock",
            locator.session_id(),
            locator.branch_id()
        ));
    Ok(if holder_is_live(&lock)? {
        "working (live owner present)"
    } else {
        "recovery required"
    })
}

/// Probe only an existing branch holder lock; never create a file or claim a lease.
fn holder_is_live(path: &Path) -> CliResult<bool> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(CliFailure::new(format!(
                "runtime holder inspection unavailable: {error}"
            )));
        }
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(error) => Err(CliFailure::new(format!(
            "runtime holder inspection unavailable: {error}"
        ))),
    }
}

/// Inspects the root's persisted tree and each child locator. Tree status and
/// canonical checkpoint status are separate facts: a quiescent child checkpoint
/// does not establish that its node retired or that its effects are settled.
pub(crate) fn show_child_recovery_status(
    paths: &crate::home::paths::PlakPaths,
    record: &product_sessions::SessionRecord,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    show_child_recovery_status_with(paths, record, stdout, |locator| {
        inspect_locator(paths, locator)
    })
}

fn show_child_recovery_status_with(
    paths: &crate::home::paths::PlakPaths,
    record: &product_sessions::SessionRecord,
    stdout: &mut dyn io::Write,
    mut inspect: impl FnMut(
        SessionLocator,
    ) -> CliResult<(SessionLocator, Option<RecoveryRequiredReport>)>,
) -> CliResult<()> {
    writeln!(stdout, "\nTopology children (durable snapshot)")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let (tree_id, observed) = crate::topology_product::inspect_session_tree(paths, record)?;
    match observed {
        TreeInspection::StoreAbsent => {
            writeln!(
                stdout,
                "  Tree store absent; child recovery was not inspected"
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
        }
        TreeInspection::TreeAbsent => {
            writeln!(
                stdout,
                "  No persisted tree at lineage head {tree_id}; child recovery was not inspected"
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
        }
        TreeInspection::Tree(tree) => {
            let tree = tree.rows();
            writeln!(stdout, "  Tree           {tree_id}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            for candidate in tree.candidates().values() {
                if let Some(commit) = &candidate.merged_at {
                    writeln!(
                        stdout,
                        "  Candidate      {} merged at {}",
                        crate::cli_output::safe_terminal_text(candidate.candidate_id.as_str(), 96),
                        crate::cli_output::safe_terminal_text(commit, 96)
                    )
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                }
            }
            let mut count = 0_usize;
            let mut incomplete = false;
            for node in tree
                .nodes()
                .values()
                .filter(|node| node.parent_agent_id.is_some())
            {
                count += 1;
                let attempt = tree.attempt(&node.agent_id);
                let child_id = crate::cli_output::safe_terminal_text(&node.agent_id, 64);
                writeln!(
                    stdout,
                    "  Child          {} node={:?} attempt={}",
                    child_id,
                    node.status,
                    attempt.map_or_else(
                        || "unavailable".to_owned(),
                        |row| format!("{:?}", row.status)
                    )
                )
                .map_err(|_| CliFailure::new("stdout failed"))?;
                let Some(locator_text) = node.session_locator.as_deref() else {
                    writeln!(stdout, "    Canonical    locator unavailable")
                        .map_err(|_| CliFailure::new("stdout failed"))?;
                    incomplete = true;
                    continue;
                };
                let Ok(recorded) = locator_text.parse::<SessionLocator>() else {
                    writeln!(stdout, "    Canonical    invalid recorded locator")
                        .map_err(|_| CliFailure::new("stdout failed"))?;
                    incomplete = true;
                    continue;
                };
                writeln!(stdout, "    Recorded     {recorded}")
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                let (current, recovery) = match inspect(recorded) {
                    Ok(result) => result,
                    Err(error) => {
                        writeln!(
                            stdout,
                            "    Canonical    inspection unavailable: {}",
                            crate::cli_output::safe_terminal_text(error.message(), 160)
                        )
                        .map_err(|_| CliFailure::new("stdout failed"))?;
                        incomplete = true;
                        continue;
                    }
                };
                if current != recorded {
                    writeln!(
                        stdout,
                        "    Current      {current} (recorded locator is stale; unchanged)"
                    )
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                }
                match recovery {
                    None => writeln!(
                        stdout,
                        "    Canonical    quiescent at {current}; node retirement is separate"
                    )
                    .map_err(|_| CliFailure::new("stdout failed"))?,
                    Some(report) => {
                        let status = working_status(paths, report.locator())?;
                        writeln!(stdout, "    Canonical    {status}")
                            .and_then(|()| {
                                writeln!(stdout, "    Phase        {:?}", report.phase())
                            })
                            .map_err(|_| CliFailure::new("stdout failed"))?;
                        for obligation in report.obligations() {
                            writeln!(
                                stdout,
                                "    Obligation   operation={} action={:?} request_identity={}",
                                obligation.operation_id(),
                                obligation.action(),
                                obligation.request_identity()
                            )
                            .map_err(|_| CliFailure::new("stdout failed"))?;
                        }
                    }
                }
            }
            if count == 0 {
                writeln!(stdout, "  Children       none in this persisted tree")
                    .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            if incomplete {
                return Err(CliFailure::recovery_required(
                    "one or more child recovery heads could not be inspected",
                ));
            }
        }
    }
    Ok(())
}

pub fn load_exact_product_session(
    paths: &crate::home::paths::PlakPaths,
    store: &dyn crate::store::session_records::SessionRecordStore,
    session_id: &str,
) -> CliResult<product_sessions::SessionRecord> {
    let cwd = env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    let record = (|| {
        let resolved = resolve_workspace_session_id(store, session_id)?;
        store.load(&resolved).map_err(CliFailure::from)
    })()
    .map_err(|error| {
        if session_absent(error.message()) {
            crate::home::state::incompatible_session_refusal(paths, session_id)
                .map_or(error, CliFailure::new)
        } else {
            error
        }
    })?;
    let requested = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let recorded = std::fs::canonicalize(&record.workspace_root)
        .unwrap_or_else(|_| record.workspace_root.clone());
    if requested != recorded {
        return Err(CliFailure::new(format!(
            "session {session_id} belongs to workspace {}; run this command from there",
            recorded.display()
        )));
    }
    if record.plakous_locator.is_none() {
        return Err(CliFailure::new(
            "session has no exact canonical Plakous locator",
        ));
    }
    Ok(record)
}

/// Resolves the short id `plak resume`, the picker and the exit line print,
/// accepted wherever a session id is, among this workspace's sessions.
pub(crate) fn resolve_workspace_session_id(
    store: &dyn crate::store::session_records::SessionRecordStore,
    requested: &str,
) -> CliResult<String> {
    resolve_session_id(requested, |cwd| {
        store
            .list_for_workspace(cwd)
            .map(|sessions| sessions.into_iter().map(|session| session.id).collect())
    })
}

/// [`resolve_workspace_session_id`] for the read-only inspection commands,
/// which never open a writable store.
pub(crate) fn resolve_read_only_session_id(
    paths: &crate::home::paths::PlakPaths,
    requested: &str,
) -> CliResult<String> {
    resolve_session_id(requested, |cwd| {
        crate::store::ProductStore::session_ids_for_workspace_read_only(paths, cwd)
    })
    .map_err(|error| {
        if session_absent(error.message()) {
            crate::home::state::incompatible_session_refusal(paths, requested)
                .map_or(error, CliFailure::new)
        } else {
            error
        }
    })
}

/// Only absence in current authority permits a foreign-session lookup.
pub(crate) fn session_absent(message: &str) -> bool {
    message == "session not found"
        || message.starts_with("no session in this workspace starts with `")
}

fn resolve_session_id(
    requested: &str,
    workspace_ids: impl FnOnce(&Path) -> Result<Vec<String>, String>,
) -> CliResult<String> {
    if uuid::Uuid::parse_str(requested).is_ok_and(|id| id.hyphenated().to_string() == requested) {
        return Ok(requested.to_owned());
    }
    let cwd = env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    let ids = workspace_ids(&cwd).map_err(CliFailure::new)?;
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    match_session_id_prefix(requested, &ids)
        .map(str::to_owned)
        .map_err(|message| CliFailure::classified(CliFailureKind::Usage, message))
}

/// Shortest session-id prefix accepted; below it a prefix says nothing.
pub(crate) const MIN_SESSION_ID_PREFIX: usize = 4;

/// Resolves a short session id among `ids`, refusing an ambiguous or unknown one.
pub(crate) fn match_session_id_prefix<'a>(
    prefix: &str,
    ids: &[&'a str],
) -> Result<&'a str, String> {
    if prefix.chars().count() < MIN_SESSION_ID_PREFIX {
        return Err(format!(
            "session id `{prefix}` is too short; give at least {MIN_SESSION_ID_PREFIX} characters or the full id"
        ));
    }
    let mut matches = ids.iter().copied().filter(|id| id.starts_with(prefix));
    match (matches.next(), matches.count()) {
        (Some(id), 0) => Ok(id),
        (Some(_), more) => Err(format!(
            "session id `{prefix}` matches {} sessions in this workspace; give more characters",
            more + 1
        )),
        (None, _) => Err(format!(
            "no session in this workspace starts with `{prefix}`; list with `plak session list` or `plak resume`"
        )),
    }
}

pub fn confirm_session_mutation(
    operation: &str,
    session_id: &str,
    confirmed: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    if confirmed {
        return Ok(());
    }
    if !rustix::termios::isatty(io::stdin()) {
        return Err(CliFailure::classified(
            CliFailureKind::Usage,
            format!("{operation} requires interactive confirmation or explicit --yes"),
        ));
    }
    writeln!(
        stderr,
        "{operation} exact session {session_id}; source remains recoverable"
    )
    .and_then(|()| write!(stderr, "Continue? [y/N] "))
    .map_err(|_| CliFailure::new("stderr failed"))?;
    stderr
        .flush()
        .map_err(|_| CliFailure::new("stderr failed"))?;
    let response = read_confirmation(operation, stdin)?;
    if matches!(response.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(CliFailure::new(format!(
            "{operation} confirmation declined"
        )))
    }
}

/// One answer line from the terminal, without its line break.
fn read_confirmation(operation: &str, stdin: &mut dyn io::BufRead) -> CliResult<String> {
    let mut response = String::new();
    let read = stdin.read_line(&mut response).map_err(|_| {
        CliFailure::new(format!(
            "the answer to the {operation} confirmation could not be read from stdin as text"
        ))
    })?;
    if read == 0 {
        return Err(CliFailure::new(format!(
            "the {operation} confirmation needs an answer on stdin and none came"
        )));
    }
    response.truncate(response.trim_end_matches(['\n', '\r']).len());
    Ok(response)
}

/// Why `plak session delete` refused to delete a session.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SessionDeleteRefusal {
    /// A running plak owns the session (or one of its finished children): its
    /// runtime branch lease is live.
    Open,
    /// The session's agent tree has children still running or resumable.
    LiveTree { children: Vec<String> },
    /// The session's agent tree has children whose state needs recovery.
    RecoveryRequiredTree { children: Vec<String> },
    /// Other sessions continue from this one or share its runtime history.
    ForkDependency { dependents: Vec<String> },
}

impl std::fmt::Display for SessionDeleteRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => formatter.write_str("it is open in a running plak; close it there first"),
            Self::LiveTree { children } => write!(
                formatter,
                "its agent tree has child agents still running or resumable ({}); close them first",
                children.join(", ")
            ),
            Self::RecoveryRequiredTree { children } => write!(
                formatter,
                "its agent tree has child agents that need recovery ({}); resolve them with recover_agent first or delete with --abandon-children",
                children.join(", ")
            ),
            Self::ForkDependency { dependents } => write!(
                formatter,
                "sessions {} continue from it or share its runtime history",
                dependents.join(", ")
            ),
        }
    }
}

fn delete_refused(id: &str, refusal: &SessionDeleteRefusal) -> CliFailure {
    CliFailure::new(format!("session {id} cannot be deleted: {refusal}"))
}

/// What deleting one session removes besides its own runtime history: the
/// runtime sessions of its agent tree's finished children and the tree rows.
struct DeletionPlan {
    children: Vec<(String, SessionLocator)>,
    tree: Option<(String, u64)>,
    abandon: Vec<String>,
}

/// `plak session delete <id> [--yes]`: deletes the session's runtime history,
/// the runtime history of its agent tree's finished children and the tree
/// itself, then removes its product record. Every refusal is checked before
/// confirmation and again after it. Archiving the session's own runtime
/// session comes first: it fences out a running owner, so an open session is
/// refused before anything is deleted. Then the children's runtime sessions
/// are deleted, then the session's archived revision, the tree rows and the
/// product record. Each step is idempotent, so a deletion that failed part way
/// is completed by running the command again. Deletion is never refused for
/// store capacity, so it is the remedy when the session store is full.
pub(crate) fn delete_session(
    services: &crate::shell::services::Services,
    id: &str,
    confirmed: bool,
    abandon_children: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let paths = services.paths()?;
    let record = load_exact_product_session(paths, services.session_records()?.as_ref(), id)?;
    let locator = record
        .plakous_locator
        .ok_or_else(|| CliFailure::new("session has no exact canonical Plakous locator"))?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    deletion_plan(paths, &mut product, &record, locator, abandon_children)?;
    confirm_deletion(&record.id, confirmed, stdin, stderr)?;
    let runtime = RuntimeDeletion::new(services)?;
    delete_confirmed_session(
        paths,
        &mut product,
        &runtime,
        &record,
        locator,
        abandon_children,
    )?;
    writeln!(stdout, "deleted session {}", record.id).map_err(|_| CliFailure::new("stdout failed"))
}

/// The confirmed deletion of one product session, in its idempotent order.
/// `plak session delete` runs it after confirmation; `plak session gc --all`
/// runs it again for a session whose runtime session a crash left archived.
/// Returns what the runtime deletions reclaimed.
fn delete_confirmed_session(
    paths: &crate::home::paths::PlakPaths,
    product: &mut crate::store::ProductStore,
    runtime: &RuntimeDeletion,
    record: &product_sessions::SessionRecord,
    locator: SessionLocator,
    abandon_children: bool,
) -> CliResult<Reclaimed> {
    let mut plan = deletion_plan(paths, product, record, locator, abandon_children)?;
    let settled_children = plan
        .children
        .iter()
        .filter(|(id, _)| !plan.abandon.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    runtime.refuse_unsettled_children(&record.id, &settled_children)?;
    let root = runtime.archive(&record.id, locator)?;
    // Fence every unresolved child before closing its durable obligation. A
    // held lease still refuses explicit abandonment; no recovery is executed.
    let mut archived_children = std::collections::BTreeMap::new();
    for (agent_id, child) in &plan.children {
        if plan.abandon.contains(agent_id) {
            archived_children.insert(agent_id.clone(), runtime.archive(&record.id, *child)?);
        }
    }
    publish_abandoned_children(paths, &mut plan)?;
    let mut reclaimed = Reclaimed::default();
    for (agent_id, child) in &plan.children {
        let archived = match archived_children.remove(agent_id) {
            Some(archived) => Ok(archived),
            None => runtime.archive(&record.id, *child),
        };
        let child = archived
            .and_then(|archived| {
                remove_job_outputs(paths, &archived)?;
                runtime.delete(archived)
            })
            .map_err(|error| {
                CliFailure::new(format!(
                    "child agent {agent_id} of session {}: {}",
                    record.id,
                    error.message()
                ))
            })?;
        reclaimed.add(child);
    }
    remove_job_outputs(paths, &root)?;
    reclaimed.add(runtime.delete(root)?);
    if let Some((tree_id, revision)) = &plan.tree {
        crate::home::state_artifacts::remember_deleted_tree(paths, tree_id)
            .map_err(CliFailure::new)?;
        crate::store::tree::open_tree_store(paths)
            .map_err(CliFailure::new)?
            .delete(tree_id, *revision)
            .map_err(|error| {
                CliFailure::new(format!("agent tree deletion failed: {}", error.detail))
            })?;
    }
    // The tree id of a session's own tree is the session id. Deleting by that
    // id is a no-op for a fork (its acknowledgements live on the source tree)
    // and idempotent when gc retries after the tree rows are already gone.
    product
        .delete_agent_acknowledgements(&record.id)
        .map_err(CliFailure::new)?;
    crate::carry::remove_retained_archive(paths, &record.id).map_err(CliFailure::new)?;
    product
        .delete_session(&record.id, record.store_revision)
        .map_err(CliFailure::new)?;
    Ok(reclaimed)
}

fn publish_abandoned_children(
    paths: &crate::home::paths::PlakPaths,
    plan: &mut DeletionPlan,
) -> CliResult<()> {
    use plakous_hosted::agent_tree::TreeStore as _;
    if plan.abandon.is_empty() {
        return Ok(());
    }
    let (tree_id, revision) = plan
        .tree
        .as_mut()
        .ok_or_else(|| CliFailure::new("child abandonment requires this session's own tree"))?;
    let store = crate::store::tree::open_tree_store(paths).map_err(CliFailure::new)?;
    let persisted = store
        .load(tree_id)
        .map_err(|error| CliFailure::new(error.detail))?
        .ok_or_else(|| CliFailure::new("the agent tree disappeared before child abandonment"))?;
    if persisted.rows().revision != *revision {
        return Err(CliFailure::new(
            "the agent tree changed before child abandonment",
        ));
    }
    let mut tree = persisted.rows().clone();
    for agent_id in &plan.abandon {
        tree.abandon_recovery_for_delete(agent_id)
            .map_err(|error| CliFailure::new(error.message))?;
    }
    let acknowledged = store
        .save(&tree)
        .map_err(|error| CliFailure::new(error.detail))?;
    *revision = acknowledged.value();
    Ok(())
}

/// Checkpoints, objects and object bytes a deletion or prune reclaimed.
#[derive(Default)]
struct Reclaimed {
    checkpoints: u64,
    objects: u64,
    bytes: u64,
}

impl Reclaimed {
    fn add(&mut self, report: Option<plakous::sessions::DeleteSessionReport>) {
        if let Some(report) = report {
            self.checkpoints = self
                .checkpoints
                .saturating_add(report.reclaimed_checkpoints());
        }
    }

    fn merge(&mut self, other: &Self) {
        self.checkpoints = self.checkpoints.saturating_add(other.checkpoints);
        self.objects = self.objects.saturating_add(other.objects);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }
}

/// Refuses deletion while other sessions depend on this one or its agent
/// tree has a child that is live, resumable or needs recovery; otherwise
/// names the finished children and the tree that go with it. A tree reached
/// through a fork source belongs to that source and is left alone.
fn deletion_plan(
    paths: &crate::home::paths::PlakPaths,
    product: &mut crate::store::ProductStore,
    record: &product_sessions::SessionRecord,
    locator: SessionLocator,
    abandon_children: bool,
) -> CliResult<DeletionPlan> {
    let dependents = product
        .session_dependents(&record.id, locator.session_id())
        .map_err(CliFailure::new)?;
    if !dependents.is_empty() {
        return Err(delete_refused(
            &record.id,
            &SessionDeleteRefusal::ForkDependency { dependents },
        ));
    }
    let (tree_id, observed) = crate::topology_product::inspect_session_tree(paths, record)?;
    let TreeInspection::Tree(tree) = observed else {
        return Ok(DeletionPlan {
            children: Vec::new(),
            tree: None,
            abandon: Vec::new(),
        });
    };
    let mut live = Vec::new();
    let mut recovering = Vec::new();
    let mut children = Vec::new();
    let may_abandon = abandon_children && tree_id == record.id;
    for node in tree.rows().nodes().values() {
        if node.parent_agent_id.is_none() {
            continue;
        }
        let label = crate::cli_output::safe_terminal_text(&node.agent_id, 64);
        if node.status == plakous_host_protocol::topology::NodeStatus::RecoveryRequired {
            recovering.push(label.clone());
            if !may_abandon {
                continue;
            }
        } else if !node.status.is_finished() {
            live.push(label);
            continue;
        }
        if let Some(child) = &node.session_locator {
            let child = child.parse::<SessionLocator>().map_err(|_| {
                CliFailure::new(format!(
                    "child agent {label} has an invalid runtime locator"
                ))
            })?;
            children.push((label, child));
        }
    }
    if !recovering.is_empty() && !may_abandon {
        return Err(delete_refused(
            &record.id,
            &SessionDeleteRefusal::RecoveryRequiredTree {
                children: recovering,
            },
        ));
    }
    if !live.is_empty() {
        return Err(delete_refused(
            &record.id,
            &SessionDeleteRefusal::LiveTree { children: live },
        ));
    }
    if tree_id != record.id {
        return Ok(DeletionPlan {
            children: Vec::new(),
            tree: None,
            abandon: Vec::new(),
        });
    }
    Ok(DeletionPlan {
        children,
        tree: Some((tree_id, tree.rows().revision)),
        abandon: recovering,
    })
}

fn confirm_deletion(
    session_id: &str,
    confirmed: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    const OPERATION: &str = "session delete";
    if confirmed {
        return Ok(());
    }
    if !rustix::termios::isatty(io::stdin()) {
        return Err(CliFailure::classified(
            CliFailureKind::Usage,
            format!("{OPERATION} requires interactive confirmation or explicit --yes"),
        ));
    }
    writeln!(
        stderr,
        "delete session {session_id}, its finished child agents and all of their retained history; this cannot be undone"
    )
    .and_then(|()| write!(stderr, "Continue? [y/N] "))
    .and_then(|()| stderr.flush())
    .map_err(|_| CliFailure::new("stderr failed"))?;
    let response = read_confirmation(OPERATION, stdin)?;
    if matches!(response.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(CliFailure::new(format!(
            "{OPERATION} confirmation declined"
        )))
    }
}

/// Removes an archived runtime session's job output files before its deletion,
/// so a failed removal leaves the session for a retried delete.
fn remove_job_outputs(
    paths: &crate::home::paths::PlakPaths,
    archive: &RuntimeArchive,
) -> CliResult<()> {
    let session_id = match archive {
        RuntimeArchive::Archived(report) => report.session_id(),
        RuntimeArchive::Earlier(session_id) => *session_id,
    };
    match std::fs::remove_dir_all(paths.job_outputs().join(session_id.to_string())) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliFailure::new(format!(
            "job output files of runtime session {session_id} could not be removed: {error}"
        ))),
    }
}

/// A runtime session after its archive step: archived now at an exact
/// revision, or already archived by an earlier interrupted run (or gone).
#[derive(Clone, Copy)]
enum RuntimeArchive {
    Archived(plakous::sessions::ArchiveSessionReport),
    Earlier(plakous::model::SessionId),
}

/// The runtime half of a session deletion, through the Plakous facade.
struct RuntimeDeletion {
    sessions: std::sync::Arc<AgentSessions>,
    executor: plakous::application::ApplicationExecutor,
}

impl RuntimeDeletion {
    fn new(services: &crate::shell::services::Services) -> CliResult<Self> {
        Ok(Self {
            sessions: services.state_roots()?.runtime_sessions()?,
            executor: plakous::application::ApplicationExecutor::new()
                .map_err(|_| CliFailure::new("application executor unavailable"))?,
        })
    }

    fn run<T>(
        &self,
        future: impl Future<Output = Result<T, plakous::sessions::AgentSessionsError>>,
    ) -> CliResult<Result<T, plakous::sessions::AgentSessionsError>> {
        self.executor
            .block_on(future)
            .map_err(|_| CliFailure::new("application execution failed"))
    }

    /// Refuses, before anything is archived, a finished child whose runtime
    /// head still carries an unsettled turn: the tree row alone does not
    /// prove the child's runtime session is at rest.
    fn refuse_unsettled_children(
        &self,
        session_id: &str,
        children: &[(String, SessionLocator)],
    ) -> CliResult<()> {
        let mut unsettled = Vec::new();
        for (agent_id, child) in children {
            let mut head = *child;
            loop {
                match self.run(self.sessions.head_is_quiescent(head))? {
                    Ok(true) => break,
                    Ok(false) => {
                        unsettled.push(agent_id.clone());
                        break;
                    }
                    // Gone, or archived by an earlier interrupted run.
                    Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => break,
                    Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                        let current = error.current_locator().ok_or_else(|| {
                            CliFailure::new("stale canonical locator did not name the current head")
                        })?;
                        if current == head {
                            return Err(CliFailure::new(format!(
                                "child agent {agent_id} runtime head could not be read: {error}"
                            )));
                        }
                        head = current;
                    }
                    Err(error) => {
                        return Err(CliFailure::new(format!(
                            "child agent {agent_id} runtime head could not be read: {error}"
                        )));
                    }
                }
            }
        }
        if unsettled.is_empty() {
            return Ok(());
        }
        Err(delete_refused(
            session_id,
            &SessionDeleteRefusal::LiveTree {
                children: unsettled,
            },
        ))
    }

    /// Archives the runtime session at its current head. A live owner's lease
    /// refuses it as open; a session an earlier run already archived (or
    /// deleted) is carried on to its deletion.
    fn archive(&self, session_id: &str, recorded: SessionLocator) -> CliResult<RuntimeArchive> {
        let mut head = recorded;
        let mut stale_retried = false;
        loop {
            match self.run(self.sessions.archive(head))? {
                Ok(report) => return Ok(RuntimeArchive::Archived(report)),
                Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => {
                    return Ok(RuntimeArchive::Earlier(recorded.session_id()));
                }
                Err(error) if error.kind() == AgentSessionsErrorKind::Ownership => {
                    return Err(delete_refused(session_id, &SessionDeleteRefusal::Open));
                }
                Err(error)
                    if error.kind() == AgentSessionsErrorKind::StaleLocator && !stale_retried =>
                {
                    stale_retried = true;
                    head = error.current_locator().ok_or_else(|| {
                        CliFailure::new("stale canonical locator did not name the current head")
                    })?;
                }
                Err(error) => {
                    return Err(CliFailure::new(format!(
                        "runtime session archive failed: {error}"
                    )));
                }
            }
        }
    }

    /// Deletes exactly the archived revision. A session archived by an earlier
    /// run is found among the archived sessions; one the store no longer
    /// retains at all is already deleted and reports nothing.
    fn delete(
        &self,
        archive: RuntimeArchive,
    ) -> CliResult<Option<plakous::sessions::DeleteSessionReport>> {
        let deleted = match archive {
            RuntimeArchive::Archived(report) => self
                .run(
                    self.sessions
                        .delete(report.session_id(), report.archived_revision()),
                )?
                .map(Some),
            RuntimeArchive::Earlier(session_id) => {
                self.run(self.sessions.delete_archived(session_id))?
            }
        };
        deleted
            .map_err(|error| CliFailure::new(format!("runtime session deletion failed: {error}")))
    }
}

/// What `plak session gc` reclaimed and what it left alone.
#[derive(Default)]
struct GcTally {
    pruned_sessions: u64,
    reclaimed: Reclaimed,
    finished_deletes: u64,
    skipped: Vec<String>,
}

/// `plak session gc <id>` or `plak session gc --all`: reclaims superseded
/// checkpoint history of quiescent sessions no running plak holds; `--all`
/// covers every session in this home and first finishes deletes a crash left
/// part way, through the same path as `plak session delete`. Pruning keeps
/// every head, so nothing a session can still show, resume or fork from goes
/// away.
pub(crate) fn gc_sessions(
    services: &crate::shell::services::Services,
    id: Option<&str>,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let paths = services.paths()?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let runtime = RuntimeDeletion::new(services)?;
    let mut tally = GcTally::default();
    let records = services.session_records()?;
    let targets: Vec<product_sessions::SessionRecord> = if let Some(id) = id {
        let record = load_exact_product_session(paths, records.as_ref(), id)?;
        if record.plakous_locator.is_none() {
            return Err(CliFailure::new(
                "session has no exact canonical Plakous locator",
            ));
        }
        vec![record]
    } else {
        finish_interrupted_deletes(services, &mut product, &runtime, &mut tally)?;
        product
            .session_locators()
            .map_err(CliFailure::new)?
            .into_iter()
            .filter(|(_, locator)| locator.is_some())
            .map(|(id, _)| records.load(&id).map_err(CliFailure::from))
            .collect::<CliResult<_>>()?
    };
    for record in targets {
        prune_session_history(&runtime, &records, &record, &mut tally)?;
    }
    let report = runtime
        .run(runtime.sessions.reclaim())?
        .map_err(|error| CliFailure::new(error.to_string()))?;
    tally.reclaimed.objects = tally.reclaimed.objects.saturating_add(report.objects());
    tally.reclaimed.bytes = tally.reclaimed.bytes.saturating_add(report.object_bytes());
    write_gc_tally(stdout, &tally).map_err(|_| CliFailure::new("stdout failed"))
}

/// Prunes one session at the head its views name after a lease-free projector
/// pass to the feed tail (spec 02 §2.5.3). A workflow request with no feed
/// fact still names its source checkpoint, which a prune could reclaim, so
/// that session is skipped until an open settles or withdraws the request.
fn prune_session_history(
    runtime: &RuntimeDeletion,
    records: &std::sync::Arc<dyn crate::store::session_records::SessionRecordStore>,
    record: &product_sessions::SessionRecord,
    tally: &mut GcTally,
) -> CliResult<()> {
    use plakous::sessions::PruneHistoryOutcome;

    let session_id = record.id.as_str();
    let projected = runtime
        .executor
        .block_on(product_host::project_session(
            &runtime.sessions,
            records,
            record,
        ))
        .map_err(|_| CliFailure::new("application execution failed"))?;
    let head = match projected {
        Ok(Some(projected)) if !projected.pending_workflows.is_empty() => {
            tally
                .skipped
                .push(format!("{session_id}: workflow pending"));
            return Ok(());
        }
        Ok(Some(projected)) => projected.head,
        Ok(None) => return Ok(()),
        Err(error) => {
            tally.skipped.push(format!("{session_id}: {error}"));
            return Ok(());
        }
    };
    let skip = match runtime.run(runtime.sessions.prune_history(head))? {
        Ok(PruneHistoryOutcome::Pruned(report)) => {
            if report.pruned_checkpoints() > 0 {
                tally.pruned_sessions += 1;
            }
            tally.reclaimed.merge(&Reclaimed {
                checkpoints: report.pruned_checkpoints(),
                objects: 0,
                bytes: 0,
            });
            return Ok(());
        }
        Ok(PruneHistoryOutcome::RecoveryRequired) => {
            "its turn is unfinished; run `plak session recover` first".to_owned()
        }
        Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
            "its head moved while gc ran; run it again".to_owned()
        }
        Err(error) if error.kind() == AgentSessionsErrorKind::Ownership => {
            "it is open in a running plak".to_owned()
        }
        Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => {
            "its runtime session is gone; finish its deletion with `plak session delete`".to_owned()
        }
        Err(error) if error.kind() == AgentSessionsErrorKind::Pruned => {
            "its recorded head was reclaimed; open the session first".to_owned()
        }
        Err(error) if error.kind() == AgentSessionsErrorKind::StorageFull => {
            format!(
                "{error}; free disk space, or reclaim unused state with `plak session gc --all` or `plak session delete <id>`"
            )
        }
        Err(error) => error.to_string(),
    };
    tally.skipped.push(format!("{session_id}: {skip}"));
    Ok(())
}

/// Finishes every delete a crash cut short. Archival is only ever the first
/// half of a delete, so a runtime session left archived is one: a product
/// record naming it is deleted again through `delete_confirmed_session`, and
/// an archived runtime session no record names (its record is already gone)
/// is deleted at its archived revision. A delete that refuses is reported as
/// skipped.
fn finish_interrupted_deletes(
    services: &crate::shell::services::Services,
    product: &mut crate::store::ProductStore,
    runtime: &RuntimeDeletion,
    tally: &mut GcTally,
) -> CliResult<()> {
    let archived = runtime
        .run(runtime.sessions.archived_sessions())?
        .map_err(|error| CliFailure::new(format!("archived session listing failed: {error}")))?;
    if archived.is_empty() {
        return Ok(());
    }
    let paths = services.paths()?;
    let records = services.session_records()?;
    let locators = product.session_locators().map_err(CliFailure::new)?;
    for archived in archived {
        let named: Vec<(&String, SessionLocator)> = locators
            .iter()
            .filter_map(|(id, locator)| {
                locator
                    .filter(|locator| locator.session_id() == archived.session_id())
                    .map(|locator| (id, locator))
            })
            .collect();
        let mut reclaimed = Reclaimed::default();
        let finished = if named.is_empty() {
            // Gone by now when a record deleted above took it as its child.
            runtime
                .delete(RuntimeArchive::Earlier(archived.session_id()))
                .map(|report| {
                    let found = report.is_some();
                    reclaimed.add(report);
                    found
                })
        } else {
            named
                .into_iter()
                .try_for_each(|(id, locator)| {
                    let record = records.load(id).map_err(CliFailure::from)?;
                    delete_confirmed_session(paths, product, runtime, &record, locator, false)
                        .map(|deleted| reclaimed.merge(&deleted))
                })
                .map(|()| true)
        };
        match finished {
            Ok(found) => {
                tally.finished_deletes += u64::from(found);
                tally.reclaimed.merge(&reclaimed);
            }
            Err(error) => tally.skipped.push(format!(
                "archived runtime session {}: {}",
                archived.session_id(),
                error.message()
            )),
        }
    }
    Ok(())
}

fn write_gc_tally(stdout: &mut dyn io::Write, tally: &GcTally) -> io::Result<()> {
    writeln!(
        stdout,
        "reclaimed {} checkpoints, {} objects, {} bytes ({} sessions pruned, {} interrupted deletes finished)",
        tally.reclaimed.checkpoints,
        tally.reclaimed.objects,
        tally.reclaimed.bytes,
        tally.pruned_sessions,
        tally.finished_deletes
    )?;
    for skipped in &tally.skipped {
        writeln!(stdout, "skipped {skipped}")?;
    }
    Ok(())
}

/// `plak session export <id> [--out <path>]`: writes the session's portable
/// document, read-only, and prints where.
pub(crate) fn export_session(
    services: &crate::shell::services::Services,
    id: &str,
    out: Option<PathBuf>,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let paths = services.paths()?;
    let record = load_exact_product_session(paths, services.session_records()?.as_ref(), id)?;
    let document = crate::carry::export_session(paths, &record).map_err(|refusal| {
        CliFailure::new(format!(
            "session {} cannot be exported: {refusal}",
            record.id
        ))
    })?;
    let path = match out {
        Some(path) => path,
        None => crate::carry::new_carry_file(paths, &record.id).map_err(CliFailure::new)?,
    };
    crate::carry::write_document(&path, &document).map_err(CliFailure::new)?;
    writeln!(stdout, "{}", path.display()).map_err(|_| CliFailure::new("stdout failed"))
}

/// Creates a new session from the export document at `path` and returns its
/// record and the note that says where it came from. The document is left in
/// place; the caller decides whether it is spent. A document whose session
/// was already imported is refused, naming the session it became.
pub(crate) fn import_session(
    services: &crate::shell::services::Services,
    path: &Path,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    let document =
        crate::carry::read_document(path).map_err(|error| CliFailure::new(error.to_string()))?;
    import_document(services, document, false)
}

pub(crate) struct PreparedImport {
    carried: crate::carry::CarriedImport,
    estimated_tokens: u64,
    messages: usize,
}
impl PreparedImport {
    pub(crate) fn new(
        document: crate::carry::CarryDocument,
        local_handoff: bool,
    ) -> CliResult<Self> {
        let estimated_tokens = document.estimated_tokens();
        let messages = document.message_count();
        let carried = document
            .into_import_with_replay(local_handoff)
            .map_err(|e| CliFailure::new(e.to_string()))?;
        Ok(Self {
            carried,
            estimated_tokens,
            messages,
        })
    }
}
pub(crate) fn import_document(
    services: &crate::shell::services::Services,
    document: crate::carry::CarryDocument,
    local_handoff: bool,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    import_prepared(services, PreparedImport::new(document, local_handoff)?)
}
pub(crate) fn import_prepared(
    services: &crate::shell::services::Services,
    prepared: PreparedImport,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    let PreparedImport {
        carried,
        estimated_tokens,
        messages,
    } = prepared;
    let crate::carry::CarriedImport {
        record,
        history,
        mut note,
        archive,
    } = carried;
    let paths = crate::home::paths::PlakPaths::for_workspace(
        services.paths()?.home().to_path_buf(),
        &record.workspace_root,
    );
    product_config::SessionPolicy::load(&paths).map_err(CliFailure::new)?;
    let (source_id, source_key) = record
        .carried_forward_from()
        .ok_or_else(|| CliFailure::new("missing carry lineage"))?;
    let (source_id, source_key) = (source_id.to_owned(), source_key.to_owned());
    if let Some(existing) = crate::carry::already_carried(
        services.session_records()?.as_ref(),
        &record.workspace_root,
        &source_id,
        &source_key,
    )
    .map_err(CliFailure::new)?
    {
        return Err(CliFailure::new(format!(
            "session {source_id} ({source_key}) was already carried forward as {}; resume it with `plak --resume {}`",
            existing.id, existing.id
        )));
    }
    record.validate_for_store().map_err(CliFailure::new)?;
    let mut config = attached_host_config(services, record)?;
    config.seed_history = Some(history);
    let (record, notes) =
        product_host::create_seeded_session_with_preflight(config, move |window| {
            let capacity = carry_capacity_from_window(window, estimated_tokens, messages);
            let message = capacity.warning().unwrap_or_else(|| {
                if capacity.admitted_input.is_none() {
                    "destination window unknown; carry will be checked at first prompt".to_owned()
                } else {
                    String::new()
                }
            });
            if !message.is_empty() {
                writeln!(
                    io::stderr(),
                    "warning: {}",
                    crate::cli_output::safe_terminal_block(&message)
                )
                .map_err(|_| CliFailure::new("stderr failed"))?;
            }
            Ok(())
        })?;
    let projected = (|| -> CliResult<()> {
        let executor = plakous::application::ApplicationExecutor::new()
            .map_err(|_| CliFailure::new("application executor failed"))?;
        let sessions = services.state_roots()?.runtime_sessions()?;
        let records = services.session_records()?;
        executor
            .block_on(product_host::project_session(&sessions, &records, &record))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
        Ok(())
    })();
    if projected.is_err() {
        note.push_str(&format!(
            "; warning: session {} was imported but its views need a projector pass; open it once",
            record.id
        ));
    }
    if let Some(archive) = &archive
        && let Err(error) =
            crate::carry::write_retained_archive(services.paths()?, &record.id, archive)
    {
        note = note.replace(
            "with the readable archive retained only as untrusted reference data",
            "without a retained readable archive",
        );
        note.push_str(&format!(
            "; warning: session {} was created without its retained reference: {error}",
            record.id
        ));
    }
    if !notes.trim().is_empty() {
        let _ = io::Write::write_all(
            &mut io::stderr(),
            format!("{}\n", notes.trim_end()).as_bytes(),
        );
    }
    Ok((record, note))
}

/// The one-shot attach uses the composition root's record service and sinks.
pub(crate) fn run_attached_action(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
    action: crate::surface_protocol::UserAction,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let mut config = attached_host_config(services, record)?;
    config.attached_one_shot = true;
    run_attached_config(config, action, stdout)
}

/// A host config for `record` under its own exact binding, as a one-shot
/// command attaches it.
fn carry_capacity_from_window(
    window: plakous::sessions::SessionInputWindow,
    estimated_tokens: u64,
    messages: usize,
) -> crate::carry::CarryCapacityProbe {
    crate::carry::CarryCapacityProbe {
        route: window.route,
        total_window: window.total_window,
        admitted_input: window.admitted_input,
        estimated_tokens,
        messages,
    }
}
pub(crate) fn probe_carry_capacity(
    services: &crate::shell::services::Services,
    path: &Path,
) -> CliResult<crate::carry::CarryCapacityProbe> {
    let document = crate::carry::read_document(path).map_err(|e| CliFailure::new(e.to_string()))?;
    probe_prepared_capacity(services, PreparedImport::new(document, false)?)
}
pub(crate) fn probe_prepared_capacity(
    services: &crate::shell::services::Services,
    prepared: PreparedImport,
) -> CliResult<crate::carry::CarryCapacityProbe> {
    let config = attached_host_config(services, prepared.carried.record)?;
    Ok(carry_capacity_from_window(
        product_host::carry_input_window(&config)?,
        prepared.estimated_tokens,
        prepared.messages,
    ))
}

fn attached_host_config(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
) -> CliResult<product_host::HostConfig<crate::surface_protocol::PlakExtension>> {
    let paths = &crate::home::paths::PlakPaths::for_workspace(
        services.paths()?.home().to_path_buf(),
        &record.workspace_root,
    );
    product_config::SessionPolicy::load(paths).map_err(CliFailure::new)?;
    let cwd = env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    // The repository gate's offline model; see `crate::test_hooks`.
    #[cfg(feature = "test-hooks")]
    if let Some(selection) = crate::test_hooks::selected() {
        let state_roots = services.state_roots()?;
        let (effort, explicit) = product_host::effort_from_config(record.effort.as_deref());
        return selection
            .and_then(|selection| {
                selection.host_config(
                    crate::test_hooks::OfflineFace::OneShot,
                    paths,
                    &state_roots,
                    record,
                    effort,
                    explicit,
                    &cwd,
                    &plakous_host_protocol::views::ReadyCatalog::default(),
                    None,
                )
            })
            .map_err(CliFailure::new);
    }
    let account = record
        .account
        .clone()
        .ok_or_else(|| CliFailure::new("session has no exact account binding"))?;
    let model = record
        .model
        .clone()
        .ok_or_else(|| CliFailure::new("session has no exact model binding"))?;
    let presentation = tui_presentation_bundle(paths)?;
    let ready_catalog = &presentation.sections;
    let (effort, explicit) = product_host::effort_from_config(record.effort.as_deref());
    if !attached_target_accepts_effort(
        &record.target_selection,
        ready_catalog,
        &account,
        &model,
        explicit,
    ) {
        return Err(CliFailure::new("session has no exact effort binding"));
    }
    let state_roots = services.state_roots()?;
    build_host_config(
        paths,
        &state_roots,
        record,
        Some(&account),
        Some(&model),
        effort,
        explicit,
        &cwd,
        ready_catalog,
        None,
        &presentation.native_models,
    )
    .map_err(CliFailure::new)
}

fn run_attached_config(
    config: product_host::HostConfig<crate::surface_protocol::PlakExtension>,
    action: crate::surface_protocol::UserAction,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let (actions, events, worker) = product_host::spawn_host_worker(config)?;
    actions
        .send(action)
        .map_err(|_| CliFailure::new("attached session command channel closed"))?;
    // Closing the sole producer drains the already-admitted ordinary action
    // before the host exits. An urgent Quit could overtake and lose it.
    drop(actions);
    join_host_worker_bounded(worker, HOST_WORKER_JOIN_TIMEOUT, "attached session")?;
    let mut failure = None;
    for event in events.try_iter() {
        match event {
            crate::surface_protocol::HostEvent::Usage(usage) => {
                let prepared = usage.prepared_input.map_or_else(
                    || "unavailable".to_owned(),
                    |value| match value {
                        plakous_host_protocol::views::presentation::PreparedInputUsage::Exact(tokens) => {
                            format!("exact:{tokens}")
                        }
                        plakous_host_protocol::views::presentation::PreparedInputUsage::UpperBound(tokens) => {
                            format!("upper-bound:{tokens}")
                        }
                    },
                );
                writeln!(
                    stdout,
                    "context_status={} prepared_input={} total_context_window={} admitted_input_ceiling={}",
                    usage.status,
                    prepared,
                    usage
                        .total_window
                        .map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
                    usage
                        .admitted_ceiling
                        .map_or_else(|| "unavailable".to_owned(), |value| value.tokens.to_string())
                )
                .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.tone == NoticeTone::Error =>
            {
                failure = Some(notice.text);
            }
            crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.placement() != NoticePlacement::SessionLog =>
            {
                writeln!(stdout, "{}", notice.text)
                    .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            crate::surface_protocol::HostEvent::Status { phase, detail, .. }
                if phase == "error" =>
            {
                failure = detail.or_else(|| Some("attached session command failed".into()));
            }
            _ => {}
        }
    }
    failure.map_or(Ok(()), |message| Err(CliFailure::new(message)))
}

/// Seeds a new session from the root template. A direct API account binds the
/// composition `platform::api_composition` decides for its model, as
/// `plak run` does; with no model the composition stays unresolved.
pub fn seed_new_session_binding(
    record: &mut product_sessions::SessionRecord,
    config: &product_config::ProductConfig,
) -> CliResult<()> {
    record.account = config.default_account().map(str::to_owned);
    record.model = config.default_model().map(str::to_owned);
    if record.account.as_deref() == Some("openrouter") {
        record.effort = None;
    } else {
        record.effort = config.default_effort().map(str::to_owned);
    }
    record.composition =
        config.composition_for_tuple(record.account.as_deref(), record.model.as_deref());
    if record.account.as_deref() == Some("openrouter")
        && record.composition == product_config::ProductCompositionSelection::FollowCatalog
    {
        record.composition = product_config::ProductCompositionSelection::Direct;
    }
    if let Some(provider) = record
        .account
        .as_deref()
        .and_then(plakous_hosted::binding::PlatformProvider::for_account)
    {
        record.target_selection = product_config::ProductTargetSelection::Platform { provider };
        resolve_api_composition(record, provider)?;
    } else if record.account.as_deref() == Some("codex")
        && config.default_extra() == Some("priority")
    {
        record.target_selection = product_config::ProductTargetSelection::NativePriority;
    } else if record.account.as_deref() == Some("openrouter") {
        record.target_selection = product_config::ProductTargetSelection::OpenRouter {
            wire: product_config::ProductWireBinding::resolve(config.default_protocol()),
            routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
        };
    }
    Ok(())
}

/// Binds the composition the product's API table admits for the record's
/// model; an inadmissible selection is refused, never substituted.
pub(crate) fn resolve_api_composition(
    record: &mut product_sessions::SessionRecord,
    provider: plakous_hosted::binding::PlatformProvider,
) -> CliResult<()> {
    let Some(model) = record.model.as_deref() else {
        return Ok(());
    };
    record.composition =
        plakous_hosted::platform::api_composition(provider, model, record.composition).map_err(
            |error| {
                CliFailure::classified(
                    CliFailureKind::Usage,
                    format!("{}: {error}", provider.account()),
                )
            },
        )?;
    Ok(())
}

pub fn repair_invalid_new_session_binding(
    record: &mut product_sessions::SessionRecord,
    config: &mut product_config::ProductConfig,
) -> bool {
    let invalid = matches!(
        (record.account.as_deref(), record.model.as_deref()),
        (Some(account), Some(model))
            if super::support::admit_account_model(account, model).is_err()
    );
    if !invalid {
        return false;
    }

    record.model = None;
    record.effort = None;
    record.composition = product_config::ProductCompositionSelection::FollowCatalog;
    record.target_selection = product_config::ProductTargetSelection::Native;
    // A template cannot carry a half tuple, so the
    // incompatible root bind is dropped rather than trimmed field by field.
    config.clear_root_binding();
    true
}

#[cfg(test)]
#[path = "session_delete_tests.rs"]
mod deletion_tests;

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use plakous_hosted::topology_runtime::test_support::bootstrap;

    #[test]
    fn holder_status_probe_is_read_only_and_distinguishes_locked_from_abandoned() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("holder.lock");
        assert!(!holder_is_live(&path).unwrap());
        assert!(!path.exists());
        let holder = std::fs::File::create(&path).unwrap();
        assert!(!holder_is_live(&path).unwrap());
        rustix::fs::flock(&holder, rustix::fs::FlockOperation::LockExclusive).unwrap();
        assert!(holder_is_live(&path).unwrap());
        drop(holder);
        assert!(!holder_is_live(&path).unwrap());
        std::fs::remove_file(&path).unwrap();
        assert!(!holder_is_live(&path).unwrap());
        assert!(holder_is_live(&home.path().join("missing-parent/holder.lock")).is_ok());
        assert!(!home.path().join("missing-parent").exists());
    }

    #[test]
    fn session_show_reports_candidate_merged_at() {
        use plakous_hosted::agent_tree_protocol::{AdoptCandidateRequest, SpawnAgentRequest};
        let (_home, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
        paths.ensure_layout().unwrap();
        let record = product_sessions::SessionRecord::new(repo.path().to_path_buf());
        let mut tree = bootstrap(&record.id, repo.path().to_path_buf());
        let child = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(
                    "implement",
                    plakous_hosted::agent_tree_protocol::Tier::Mutate,
                    "editor",
                    "implement",
                )
                .with_fork(),
                None,
            )
            .unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("merged.txt"),
            "merged content",
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let id = tree
            .candidates()
            .values()
            .next()
            .unwrap()
            .candidate_id
            .clone();
        let adopted = tree
            .adopt_candidate(
                "root",
                &AdoptCandidateRequest {
                    candidate_id: id.clone(),
                    expected_target: None,
                    evidence: None,
                },
                None,
            )
            .unwrap();
        tree.bind_session_locator(&child.agent_id,
            "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003"
        ).unwrap();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        let mut output = Vec::new();
        show_child_recovery_status_with(&paths, &record, &mut output, |locator| {
            Ok((locator, None))
        })
        .unwrap();
        assert!(String::from_utf8(output).unwrap().contains(&format!(
            "Candidate      {id} merged at {}",
            adopted.target_after
        )));
    }

    #[test]
    fn child_inspection_continues_past_bad_first_locator_and_returns_incomplete() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let record = product_sessions::SessionRecord::new(paths.home().to_path_buf());
        let mut tree = bootstrap(&record.id, paths.home().to_path_buf());
        let first = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap();
        let second = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap();
        tree.bind_session_locator(&first.agent_id, "invalid locator")
            .unwrap();
        let valid = "00000000-0000-0000-0000-000000000001/\
                     00000000-0000-0000-0000-000000000002/\
                     00000000-0000-0000-0000-000000000003";
        tree.bind_session_locator(&second.agent_id, valid).unwrap();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();

        let mut output = Vec::new();
        let mut calls = Vec::new();
        let result = show_child_recovery_status_with(&paths, &record, &mut output, |locator| {
            calls.push(locator);
            Ok((locator, None))
        });
        let output = String::from_utf8(output).unwrap();
        assert!(result.unwrap_err().is_recovery_required());
        assert_eq!(calls.len(), 1, "only the later valid locator is inspected");
        assert_eq!(calls[0].to_string(), valid);
        assert!(output.contains("invalid recorded locator"));
        assert!(output.contains(&format!("Child          {}", second.agent_id)));
        assert!(output.contains(&format!("quiescent at {valid}")));
        assert!(output.find("invalid recorded locator") < output.find("quiescent at"));
    }
}
