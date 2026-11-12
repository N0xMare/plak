//! Session deletion and garbage collection.

use super::lifecycle::{LifecycleError, LifecycleRefusal};
use super::lookup::{load_exact_product_session, load_metadata_product_session};
use crate::store::sessions as product_sessions;
use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionLocator};
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use plakous_hosted::runtime as product_host;
use plakous_recursive_subagents_store::TreeInspection;
use std::io;

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

type SessionDeleteRefusal = LifecycleRefusal;

fn delete_refused(_id: &str, refusal: &SessionDeleteRefusal) -> LifecycleError {
    LifecycleError::Refused(refusal.clone())
}

fn cli_delete_error(id: &str, error: LifecycleError) -> CliFailure {
    match error {
        LifecycleError::Refused(reason) => {
            CliFailure::new(format!("session {id} cannot be deleted: {reason}"))
        }
        LifecycleError::Failure(error) => error,
    }
}

pub(super) fn delete_confirmed(
    services: &crate::shell::services::Services,
    id: &str,
    abandon_children: bool,
) -> Result<(), LifecycleError> {
    let paths = services.paths()?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    product.lifecycle_metadata(id)?;
    let record = services
        .session_records()?
        .load(id)
        .map_err(CliFailure::new)?;
    let runner_id = record
        .runner
        .as_ref()
        .map_or("local", |runner| runner.runner_id.as_str());
    if crate::terminal_registry::session_pinned(services, id, runner_id).map_err(CliFailure::new)? {
        return Err(LifecycleError::Refused(LifecycleRefusal::Open));
    }
    if let Some(successor) = product
        .carried_forward(
            &record.workspace_root,
            runner_id,
            &record.id,
            &crate::home::state::state_key(),
        )
        .map_err(CliFailure::new)?
        .filter(|successor| successor.carry_intent.is_some() && successor.plakous_locator.is_none())
    {
        return Err(delete_refused(
            id,
            &SessionDeleteRefusal::fork_dependency(vec![successor.id]),
        ));
    }
    if record.plakous_locator.is_none() && record.carry_intent.is_some() {
        return Err(LifecycleError::Refused(LifecycleRefusal::Busy {
            turn: false,
            children: Vec::new(),
            children_truncated: false,
            processes: Vec::new(),
            processes_truncated: false,
            state_unknown: true,
        }));
    }
    let locator = if let Some(locator) = record.plakous_locator {
        locator
    } else {
        let operation = plakous::sessions::SessionOperationId::new(&record.create_operation_id)
            .map_err(|error| CliFailure::new(error.to_string()))?;
        // Creation may have committed before its product locator save failed.
        // The immutable create intent names that runtime even then.
        AgentSessions::genesis_locator_for(&operation)
            .map_err(|error| CliFailure::new(error.to_string()))?
    };
    let runtime = RuntimeDeletion::new(services)?;
    delete_confirmed_session(
        paths,
        &mut product,
        &runtime,
        &record,
        locator,
        abandon_children,
    )?;
    Ok(())
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
    let record = load_metadata_product_session(paths, services.session_records()?.as_ref(), id)?;
    refuse_stranding_a_carry(services, &record)?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if let Some(locator) = record.plakous_locator {
        deletion_plan(paths, &mut product, &record, locator, abandon_children)
            .map_err(|error| cli_delete_error(&record.id, error))?;
    }
    confirm_deletion(&record.id, confirmed, stdin, stderr)?;
    if !super::lifecycle::forward_delete(services, &record.id, abandon_children)? {
        if record.plakous_locator.is_none() {
            return Err(cli_delete_error(
                &record.id,
                LifecycleError::Refused(LifecycleRefusal::Busy {
                    turn: false,
                    children: Vec::new(),
                    children_truncated: false,
                    processes: Vec::new(),
                    processes_truncated: false,
                    state_unknown: true,
                }),
            ));
        }
        super::lifecycle::delete_confirmed(services, &record.id, abandon_children)
            .map_err(|error| cli_delete_error(&record.id, error))?;
    }
    writeln!(stdout, "deleted session {}", record.id).map_err(|_| CliFailure::new("stdout failed"))
}

/// Deleting a carry's source before the carry settles would strand it.
pub(crate) fn refuse_stranding_a_carry(
    services: &crate::shell::services::Services,
    record: &product_sessions::SessionRecord,
) -> CliResult<()> {
    let mut product = crate::store::ProductStore::open(services.paths()?)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let runner_id = record
        .runner
        .as_ref()
        .map_or("local", |runner| runner.runner_id.as_str());
    match product
        .carried_forward(
            &record.workspace_root,
            runner_id,
            &record.id,
            &crate::home::state::state_key(),
        )
        .map_err(CliFailure::new)?
        .filter(|successor| successor.carry_intent.is_some() && successor.plakous_locator.is_none())
    {
        Some(successor) => Err(CliFailure::new(format!(
            "session {} has an interrupted carry into session {}; resume either one to settle it, then delete",
            record.id, successor.id
        ))),
        None => Ok(()),
    }
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
) -> Result<Reclaimed, LifecycleError> {
    let mut plan = deletion_plan(paths, product, record, locator, abandon_children)?;
    let settled_children = plan
        .children
        .iter()
        .filter(|(id, _)| !plan.abandon.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    runtime.refuse_unsettled_children(&record.id, &settled_children)?;
    let root = runtime.archive(&record.id, locator)?;
    #[cfg(test)]
    crate::store::lifecycle_crash_cut("delete_root_archived");
    // Fence every unresolved child before closing its durable obligation. A
    // held lease still refuses explicit abandonment; no recovery is executed.
    let mut archived_children = std::collections::BTreeMap::new();
    for (agent_id, child) in &plan.children {
        if plan.abandon.contains(agent_id) {
            archived_children.insert(agent_id.clone(), runtime.archive(&record.id, *child)?);
        }
    }
    publish_abandoned_children(paths, &mut plan)?;
    #[cfg(test)]
    crate::store::lifecycle_crash_cut("delete_abandon_committed");
    let mut reclaimed = Reclaimed::default();
    for (agent_id, child) in &plan.children {
        let archived = match archived_children.remove(agent_id) {
            Some(archived) => Ok(archived),
            None => runtime.archive(&record.id, *child),
        };
        let archived = archived?;
        remove_job_outputs(paths, &archived)?;
        let child = runtime.delete(archived)?;
        #[cfg(test)]
        crate::store::lifecycle_crash_cut("delete_child_deleted");
        reclaimed.add(child);
    }
    remove_job_outputs(paths, &root)?;
    reclaimed.add(runtime.delete(root)?);
    #[cfg(test)]
    crate::store::lifecycle_crash_cut("delete_runtime_deleted");
    if let Some((tree_id, revision)) = &plan.tree {
        use plakous_recursive_subagents::tree::TreeStore as _;
        crate::home::state_artifacts::remember_deleted_tree(paths, tree_id)
            .map_err(CliFailure::new)?;
        // The tree's rows and its custody record go together, under this
        // process's custody, so no other host is materializing it meanwhile.
        crate::store::tree::open_tree_store(paths)
            .map_err(CliFailure::new)?
            .delete_in_custody(tree_id, *revision)
            .map_err(|error| {
                CliFailure::new(format!("agent tree deletion failed: {}", error.detail))
            })?;
    }
    #[cfg(test)]
    crate::store::lifecycle_crash_cut("delete_tree_deleted");
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
        .save_in_custody(&tree)
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
) -> Result<DeletionPlan, LifecycleError> {
    let dependents = product
        .session_dependents(&record.id, locator.session_id())
        .map_err(CliFailure::new)?;
    if !dependents.is_empty() {
        return Err(delete_refused(
            &record.id,
            &SessionDeleteRefusal::fork_dependency(dependents),
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
        let label = node.agent_id.clone();
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
            &SessionDeleteRefusal::recovery_required_tree(recovering),
        ));
    }
    if !live.is_empty() {
        return Err(delete_refused(
            &record.id,
            &SessionDeleteRefusal::live_tree(live),
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
    ) -> Result<(), LifecycleError> {
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
                            ))
                            .into());
                        }
                        head = current;
                    }
                    Err(error) => {
                        return Err(CliFailure::new(format!(
                            "child agent {agent_id} runtime head could not be read: {error}"
                        ))
                        .into());
                    }
                }
            }
        }
        if unsettled.is_empty() {
            return Ok(());
        }
        Err(delete_refused(
            session_id,
            &SessionDeleteRefusal::live_tree(unsettled),
        ))
    }

    /// Archives the runtime session at its current head. A live owner's lease
    /// refuses it as open; a session an earlier run already archived (or
    /// deleted) is carried on to its deletion.
    fn archive(
        &self,
        session_id: &str,
        recorded: SessionLocator,
    ) -> Result<RuntimeArchive, LifecycleError> {
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
                    ))
                    .into());
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
    if let Some(id) = id {
        let record =
            load_metadata_product_session(paths, services.session_records()?.as_ref(), id)?;
        if record.archived {
            return Err(CliFailure::new(
                "session is archived; unarchive it before running gc",
            ));
        }
    }
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
        if record.archived {
            tally
                .skipped
                .push(format!("{}: archived; unarchive before gc", record.id));
            continue;
        }
        prune_session_history(&runtime, &records, &record, &mut tally)?;
    }
    let report = runtime
        .run(runtime.sessions.reclaim())?
        .map_err(|error| CliFailure::new(error.to_string()))?;
    tally.reclaimed.objects = tally.reclaimed.objects.saturating_add(report.objects());
    tally.reclaimed.bytes = tally.reclaimed.bytes.saturating_add(report.object_bytes());
    write_gc_tally(stdout, &tally).map_err(|_| CliFailure::new("stdout failed"))?;
    let names = product.session_locators().map_err(CliFailure::new)?;
    write_store_report(stdout, paths, &names).map_err(|_| CliFailure::new("stdout failed"))
}

/// The bound nearest where new work stops, relative to that stop: the one
/// that refuses new work first.
fn binding_bound(
    bounds: &[plakous::sessions::SessionStoreBoundUsage],
) -> plakous::sessions::SessionStoreBoundUsage {
    let share = |bound: &plakous::sessions::SessionStoreBoundUsage| {
        u128::from(bound.used()) * 1_000_000 / u128::from(bound.ordinary_threshold().max(1))
    };
    let mut binding = bounds[0];
    for bound in &bounds[1..] {
        if share(bound) > share(&binding) {
            binding = *bound;
        }
    }
    binding
}

/// How many sessions the gc report lists by retained bytes.
const LARGEST_SESSIONS: usize = 5;

/// What is left after gc: the store's remaining object bytes against where
/// new work stops, the sessions retaining the most, and any running plak on
/// this home that uses another build's store, which this gc did not touch.
fn write_store_report(
    stdout: &mut dyn io::Write,
    paths: &crate::home::paths::PlakPaths,
    names: &[(String, Option<SessionLocator>)],
) -> io::Result<()> {
    let store = paths.runtime_database();
    match plakous::sessions::SessionBackend::inspect_sqlite_usage(&store) {
        Ok(Some(usage)) => {
            let binding = binding_bound(usage.bounds());
            writeln!(
                stdout,
                "remaining {} {} in {}: new work stops at {}, child agents' at {}, limit {}",
                binding.used(),
                binding.bound(),
                store.display(),
                binding.ordinary_threshold(),
                binding.subordinate_threshold(),
                binding.limit()
            )?;
        }
        Ok(None) => writeln!(
            stdout,
            "remaining 0 object bytes: {} holds no sessions",
            store.display()
        )?,
        Err(error) => writeln!(stdout, "remaining bytes unavailable: {error}")?,
    }
    match plakous::sessions::SessionBackend::inspect_sqlite_session_retention(&store) {
        Ok(sessions) if !sessions.is_empty() => {
            writeln!(
                stdout,
                "largest sessions by retained object bytes (objects they share count toward each):"
            )?;
            for session in sessions.iter().take(LARGEST_SESSIONS) {
                let name = names
                    .iter()
                    .find(|(_, locator)| {
                        locator.is_some_and(|locator| locator.session_id() == session.session_id())
                    })
                    .map_or_else(
                        || {
                            format!(
                                "runtime session {} (a sub-agent's, or no product record)",
                                session.session_id()
                            )
                        },
                        |(id, _)| format!("session {id}"),
                    );
                writeln!(
                    stdout,
                    "  {} bytes, {} checkpoints: {name}",
                    session.object_bytes(),
                    session.checkpoints()
                )?;
            }
        }
        Ok(_) => {}
        Err(error) => writeln!(stdout, "per-session retention unavailable: {error}")?,
    }
    let current = crate::home::state::state_key();
    let others: Vec<String> = crate::home::state::running_state_keys(paths)
        .into_iter()
        .filter(|key| *key != current)
        .collect();
    if !others.is_empty() {
        writeln!(
            stdout,
            "note: a running plak on this home uses state {} of another build; this gc ran as build state {current} and did not touch that store; run `plak session gc` with that build to reclaim it",
            others.join(", ")
        )?;
    }
    Ok(())
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
        // A predecessor of a line stays whole until it is deleted.
        Ok(PruneHistoryOutcome::Superseded) => {
            "superseded: its agent line continues in a later session; kept as read-only history"
                .to_owned()
        }
        Ok(PruneHistoryOutcome::LineEnded) => {
            "superseded: its agent line ended when the session holding its head was deleted; kept as read-only history until it is deleted"
                .to_owned()
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
        if named
            .iter()
            .any(|(id, _)| records.load(id).is_ok_and(|record| record.archived))
        {
            continue;
        }
        let mut reclaimed = Reclaimed::default();
        let finished = if named.is_empty() {
            // Gone by now when a record deleted above took it as its child.
            runtime
                .delete(RuntimeArchive::Earlier(archived.session_id()))
                .map_err(LifecycleError::from)
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

#[cfg(test)]
#[path = "../session_delete_tests.rs"]
mod deletion_tests;
