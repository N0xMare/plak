//! Source-owned group collection has no driver, custody take, admission or native write.
use super::{Capsule, Custody, HeadContext, RunnerOwner, Tree, configuration, producer::SourceCut};
use crate::home::paths::PlakPaths;
use crate::shell::services::Services;
use crate::store::{ProductStore, sessions::SessionRecord};
use crate::{CliFailure, CliResult};
use plakous::model::{BranchId, SessionId};
use plakous::sessions::host_move::MoveInstall;
use plakous::sessions::{SessionBackend, SessionLocator};
use plakous_hosted::sessions::host_move::{MoveGroup, MoveGroups};
use plakous_recursive_subagents::tree::host_move::DormantTree;
use std::collections::{BTreeMap, BTreeSet};

mod terminals;
pub(crate) use terminals::Proof;

pub(crate) struct Snapshot {
    pub(crate) native: MoveInstall,
    pub(crate) proofs: Vec<Proof>,
}

pub(crate) struct Inventory {
    pub(crate) groups: Vec<MoveGroup>,
    operation: [u8; 32],
    records: Vec<SessionRecord>,
    trees: BTreeMap<String, Tree>,
}

fn failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(format!("host_upgrade_move_blocked: {error}"))
}

impl Inventory {
    pub(crate) fn persisted_trees(&self) -> impl Iterator<Item = &String> {
        self.trees.keys()
    }

    pub(crate) fn read(services: &Services, operation: [u8; 32]) -> CliResult<Option<Self>> {
        let paths = services.paths()?;
        let records = ProductStore::inspect_upgrade_roots(paths).map_err(failure)?;
        if records.is_empty() {
            return Ok(None);
        }
        if records.len() > 4096 {
            return Err(failure("too many carried roots"));
        }
        let mut trees = BTreeMap::new();
        let mut members = BTreeMap::<_, BTreeSet<_>>::new();
        for record in &records {
            crate::carry::upgrade::receipt::ensure_open(paths, record)?;
            let source = record
                .plakous_locator
                .ok_or_else(|| failure("a carried root has no native binding"))?;
            members
                .entry(record.node.tree_id.clone())
                .or_default()
                .insert(source.session_id());
            if let Some((rows, custody)) =
                plakous_recursive_subagents_store::SqliteTreeStore::inspect_move_tree(
                    &paths.subagents_database(),
                    &record.node.tree_id,
                )
                .map_err(failure)?
            {
                let cut = DormantTree::capture(rows.rows()).map_err(failure)?;
                for node in cut.nodes().values() {
                    if let Some(locator) = &node.session_locator {
                        let locator: SessionLocator = locator.parse().map_err(failure)?;
                        members
                            .entry(record.node.tree_id.clone())
                            .or_default()
                            .insert(locator.session_id());
                    }
                }
                trees.insert(
                    record.node.tree_id.clone(),
                    Tree {
                        cut,
                        custody: Custody {
                            fence: custody.fence,
                            host: custody.holding.host.host_id,
                            instance: custody.holding.host.instance.to_string(),
                            binary: custody.holding.host.binary,
                            machine: custody.holding.host.host_name,
                            claimed_ms: custody.holding.claimed_at_ms,
                            renewed_ms: custody.holding.renewed_at_ms,
                        },
                    },
                );
            }
        }
        let ids = members.values().flatten().copied().collect::<BTreeSet<_>>();
        let dependencies = SessionBackend::inspect_move_dependencies_sqlite(
            paths.runtime_database(),
            &ids.into_iter().collect::<Vec<_>>(),
        )
        .map_err(failure)?;
        let groups = MoveGroups::plan(
            &plakous::model::hex_lower(&operation),
            &members,
            &dependencies,
        )
        .map_err(failure)?;
        Ok(Some(Self {
            operation,
            groups: groups.groups().to_vec(),
            records,
            trees,
        }))
    }

    pub(crate) fn operation(&self, index: usize) -> [u8; 32] {
        plakous::model::sha256_bytes(&[
            b"host-move/group-operation/v1",
            &self.operation,
            self.groups[index].id().as_bytes(),
        ])
    }

    pub(crate) fn snapshot(
        &self,
        services: &Services,
        index: usize,
        runtime: &tokio::runtime::Handle,
    ) -> CliResult<Snapshot> {
        let mut source = self.unsettled(services.paths()?, index)?;
        let proofs = self.sample_terminals(services, &mut source, runtime)?;
        Ok(Snapshot {
            native: source.settled().map_err(failure)?,
            proofs,
        })
    }

    fn unsettled(
        &self,
        paths: &PlakPaths,
        index: usize,
    ) -> CliResult<plakous::sessions::host_move::MoveSource> {
        SessionBackend::inspect_move_source_unsettled_sqlite(
            paths.runtime_database(),
            &self.groups[index]
                .sessions()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
        )
        .map_err(failure)
    }

    pub(crate) fn runner_ids(&self, paths: &PlakPaths) -> CliResult<BTreeSet<String>> {
        let mut runners = BTreeSet::new();
        for index in 0..self.groups.len() {
            for session in self.unsettled(paths, index)?.sessions {
                for branch in session.branches {
                    runners.insert(self.runner_for(paths, branch.session)?);
                }
            }
        }
        Ok(runners)
    }

    pub(crate) fn capsule(
        &self,
        services: &Services,
        index: usize,
        native: &MoveInstall,
        runtime: &tokio::runtime::Handle,
    ) -> CliResult<Capsule> {
        let mut capsule = self.semantic_capsule(services, index, native, runtime)?;
        observe_owners(services, &mut capsule, runtime)?;
        Ok(capsule)
    }

    pub(crate) fn semantic_capsule(
        &self,
        services: &Services,
        index: usize,
        native: &MoveInstall,
        runtime: &tokio::runtime::Handle,
    ) -> CliResult<Capsule> {
        let paths = services.paths()?;
        let group = &self.groups[index];
        let mut products = Vec::new();
        let mut contexts = BTreeMap::new();
        for record in self
            .records
            .iter()
            .filter(|record| group.trees().contains(&record.node.tree_id))
        {
            let source = record
                .plakous_locator
                .ok_or_else(|| failure("missing product head"))?;
            let session = native
                .sessions()
                .iter()
                .find(|session| session.session.session_id() == source.session_id())
                .ok_or_else(|| failure("native group omits its product"))?;
            let sessions = services.state_roots()?.runtime_sessions()?;
            let records = services.session_records()?;
            runtime
                .block_on(plakous_hosted::runtime::project_session(
                    &sessions, &records, record,
                ))
                .map_err(failure)?;
            products.push(
                ProductStore::inspect_move_source(
                    paths,
                    &record.id,
                    plakous::sessions::feed::FeedSeq::new(session.feed_tail_floor),
                )
                .map_err(failure)?
                .document()
                .map_err(failure)?,
            );
        }
        for session in native.sessions() {
            for branch in &session.branches {
                contexts.insert(
                    (branch.session.session_id(), branch.session.branch_id()),
                    self.context(
                        paths,
                        branch.session.session_id(),
                        branch.session.branch_id(),
                        &products,
                    )?,
                );
            }
        }
        let identity = plakous_recursive_subagents::tree::custody_host();
        SourceCut {
            operation: self.operation(index),
            key: crate::home::state::state_key(),
            binary: crate::build_identity::binary_id().into(),
            host: crate::commands::runner::existing_host_id(paths)?,
            instance: identity.instance.to_string(),
            products,
            contexts,
            owners: BTreeMap::new(),
            trees: self.project_trees(services, group, runtime)?,
        }
        .capsule(native)
        .map_err(failure)
    }

    fn project_trees(
        &self,
        services: &Services,
        group: &MoveGroup,
        runtime: &tokio::runtime::Handle,
    ) -> CliResult<Vec<Tree>> {
        let sessions = services.state_roots()?.runtime_sessions()?;
        group
            .trees()
            .iter()
            .filter_map(|id| self.trees.get(id).map(|tree| (id, tree)))
            .map(|(id, tree)| {
                let rows = tree.cut.installed_rows().map_err(failure)?;
                let mut bindings = Vec::new();
                for record in self
                    .records
                    .iter()
                    .filter(|record| &record.node.tree_id == id)
                {
                    let locator = record
                        .plakous_locator
                        .ok_or_else(|| failure("source root has no native session"))?;
                    bindings.push((rows.rows().root_agent_id.clone(), locator.session_id()));
                }
                for (agent, node) in rows.rows().nodes() {
                    if agent != &rows.rows().root_agent_id
                        && let Some(locator) = &node.session_locator
                    {
                        let locator: SessionLocator = locator.parse().map_err(failure)?;
                        bindings.push((agent.clone(), locator.session_id()));
                    }
                }
                let cut = runtime
                    .block_on(plakous_hosted::sessions::host_move::project_move_tree(
                        &sessions,
                        rows.rows().clone(),
                        &bindings,
                    ))
                    .map_err(failure)?;
                Ok(Tree {
                    cut,
                    custody: tree.custody.clone(),
                })
            })
            .collect()
    }

    fn context(
        &self,
        paths: &PlakPaths,
        session: SessionId,
        branch: BranchId,
        products: &[plakous_hosted::sessions::host_move::DormantSessionDocument],
    ) -> CliResult<HeadContext> {
        if let Some((record, authority)) =
            ProductStore::inspect_move_context(paths, session, branch).map_err(failure)?
        {
            let represented = self.records.iter().find(|root| root.node.tree_id == record.node.tree_id)
                .ok_or_else(|| failure("native branch belongs to an uncarried tree; resolve that dependency with the source build before upgrading"))?;
            let mut context = root_context(represented, products)?;
            context.configuration =
                configuration::Configuration::capture(&record).map_err(failure)?;
            context.workspace.clone_from(&record.workspace_root);
            context.runner = record
                .runner
                .as_ref()
                .map_or("local", |runner| runner.runner_id.as_str())
                .into();
            context.agent.clone_from(&record.node.agent_id);
            context.plan_authority = authority;
            return Ok(context);
        }
        for (tree, source) in &self.trees {
            for node in source.cut.nodes().values() {
                if node.session_locator.as_ref().is_none_or(|locator| {
                    !locator.parse::<SessionLocator>().is_ok_and(|locator| {
                        locator.session_id() == session && locator.branch_id() == branch
                    })
                }) {
                    continue;
                }
                let record = self
                    .records
                    .iter()
                    .find(|record| &record.node.tree_id == tree)
                    .ok_or_else(|| failure("child has no represented source tree"))?;
                let mut context = root_context(record, products)?;
                let rows = source.cut.installed_rows().map_err(failure)?;
                context.configuration = configuration::Configuration::capture_child(
                    record,
                    rows.rows(),
                    &node.agent_id,
                )
                .map_err(failure)?;
                context.agent.clone_from(&node.agent_id);
                context.workspace.clone_from(&node.path);
                context.runner = node
                    .placement
                    .runner_id
                    .clone()
                    .unwrap_or_else(|| "local".into());
                context.plan_authority = format!("{}.{}", tree, node.agent_id);
                return Ok(context);
            }
        }
        Err(failure(
            "native predecessor has no represented configuration context",
        ))
    }
}

fn root_context(
    record: &SessionRecord,
    products: &[plakous_hosted::sessions::host_move::DormantSessionDocument],
) -> CliResult<HeadContext> {
    let authority = products
        .iter()
        .find(|product| product.id() == record.id)
        .and_then(|product| product.plan_authority())
        .ok_or_else(|| failure("missing source plan-state namespace"))?;
    Ok(HeadContext {
        product: record.id.clone(),
        tree: record.node.tree_id.clone(),
        agent: record.node.agent_id.clone(),
        workspace: record.workspace_root.clone(),
        runner: record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str())
            .into(),
        plan_authority: authority.into(),
        configuration: configuration::Configuration::capture(record).map_err(failure)?,
    })
}

fn observe_owners(
    services: &Services,
    capsule: &mut Capsule,
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    let mut runners = BTreeMap::<_, BTreeSet<_>>::new();
    for head in capsule.sessions.iter().flat_map(|session| &session.heads) {
        runners
            .entry(head.context.runner.clone())
            .or_default()
            .insert(head.line.clone());
    }
    let mut owners = BTreeMap::new();
    for (runner, lines) in runners {
        let lines = lines.into_iter().collect::<Vec<_>>();
        let client = if runner == "local" {
            services.local_runner_client()
        } else {
            services.runners().client(&runner)
        }
        .ok_or_else(|| failure("source runner connection is unavailable"))?;
        let statuses = runtime
            .block_on(client.lines())
            .map_err(|error| failure(format!("source runner work inspection failed: {error:?}")))?;
        for status in statuses {
            if !lines.contains(&status.line) {
                continue;
            }
            let (_, jobs) =
                plakous::tools::runner::decode_jobs_snapshot(status.jobs).map_err(failure)?;
            if status.foreground
                || jobs.iter().any(|job| {
                    matches!(
                        job.status,
                        plakous::tools::JobStatus::Running
                            | plakous::tools::JobStatus::OutcomeUnknown
                    )
                })
            {
                return Err(failure(
                    "runner work is still live or its outcome is unknown; settle it with the source build before moving this group",
                ));
            }
        }
        for batch in lines.chunks(256) {
            for floor in runtime
                .block_on(client.owner_floors(batch))
                .map_err(|error| failure(format!("source runner witness read failed: {error:?}")))?
            {
                let Some(owner) = floor.owner else { continue };
                let head = capsule
                    .sessions
                    .iter()
                    .flat_map(|session| &session.heads)
                    .find(|head| {
                        head.source
                            .parse::<SessionLocator>()
                            .is_ok_and(|source| source.branch_id().to_string() == owner.branch)
                    })
                    .ok_or_else(|| failure("runner witness names an unrepresented branch"))?;
                let source: SessionLocator = head.source.parse().map_err(failure)?;
                let witness = RunnerOwner {
                    runner: runner.clone(),
                    host: capsule.source_host.clone(),
                    line: owner.line,
                    generation: owner.generation,
                    session: source.session_id().to_string(),
                    branch: owner.branch,
                    fence: owner.fence,
                };
                let line = capsule
                    .lines
                    .iter()
                    .find(|line| line.id == witness.line)
                    .ok_or_else(|| failure("runner witness names an unexpected line"))?;
                capsule
                    .validate_runner_owner(line, &witness)
                    .map_err(failure)?;
                if capsule.current_context(line).map_err(failure)?.runner == runner {
                    owners.insert(line.id.clone(), witness);
                }
            }
        }
    }
    for line in &mut capsule.lines {
        line.source_owner = owners.remove(&line.id);
    }
    Ok(())
}

/// The retained executable resolves only its own key's native retirement.
/// It never binds a runner or opens an execution driver.
pub(crate) fn finish_source(services: &Services, id: uuid::Uuid) -> CliResult<()> {
    use crate::carry::upgrade::receipt::{self, Phase};
    let paths = services.paths()?;
    let _lock = receipt::publication::lock(paths)?;
    let mut receipt = receipt::read(
        &receipt::root(paths)
            .join(id.to_string())
            .join("receipt.json"),
    )?;
    if receipt.source_key != crate::home::state::state_key()
        || receipt.source_binary != crate::build_identity::binary_id()
        || !matches!(receipt.phase, Phase::Committed | Phase::Acknowledged)
    {
        return Err(failure(
            "source retirement requires its exact retained producer and committed decision",
        ));
    }
    // Re-establish the durable global decision before making native retirement permanent.
    receipt.save(paths)?;
    let sessions = plakous::sessions::AgentSessions::new(
        SessionBackend::sqlite(paths.runtime_database()).map_err(failure)?,
    )
    .map_err(failure)?;
    let executor = plakous::application::ApplicationExecutor::new().map_err(failure)?;
    for group in &mut receipt.groups {
        if group.source_retired {
            continue;
        }
        let digest = group
            .source_digest_value()?
            .ok_or_else(|| failure("missing source preparation digest"))?;
        let operation =
            plakous::sessions::host_move::MoveOperation::new(group.operation).map_err(failure)?;
        executor
            .block_on(sessions.finish_move(
                operation,
                digest,
                plakous::sessions::host_move::MoveDisposition::Commit,
            ))
            .map_err(failure)?
            .map_err(failure)?;
        group.source_retired = true;
        crate::carry::upgrade::crash_cut("move-after-native-retirement");
    }
    receipt.save(paths)
}

pub(crate) fn withdraw(
    paths: &PlakPaths,
    receipt: &mut crate::carry::upgrade::receipt::Receipt,
) -> CliResult<()> {
    use plakous::sessions::host_move::{MoveDisposition, MoveOperation};
    if receipt.groups.is_empty() {
        return Ok(());
    }
    if receipt.source_key != crate::home::state::state_key()
        || !matches!(
            receipt.phase,
            crate::carry::upgrade::receipt::Phase::Prepared
                | crate::carry::upgrade::receipt::Phase::Aborted
        )
        || !paths.runtime_database().is_file()
    {
        return Err(failure(
            "native preparation cleanup requires its own intact source key",
        ));
    }
    let sessions = plakous::sessions::AgentSessions::new(
        SessionBackend::sqlite(paths.runtime_database()).map_err(failure)?,
    )
    .map_err(failure)?;
    let executor = plakous::application::ApplicationExecutor::new().map_err(failure)?;
    for index in 0..receipt.groups.len() {
        let group = &receipt.groups[index];
        let operation = MoveOperation::new(group.operation).map_err(failure)?;
        let digest = match group.source_digest_value()? {
            Some(digest) => digest,
            None => match executor
                .block_on(sessions.recover_move(operation))
                .map_err(failure)?
            {
                Ok(native) => {
                    let digest = native.digest();
                    // A repeated source startup must still know the exact batch
                    // after Abort makes recover_move refuse its disposed state.
                    receipt.groups[index].source_digest =
                        Some(plakous::model::hex_lower(digest.bytes()));
                    receipt.save(paths)?;
                    digest
                }
                Err(error)
                    if error.kind() == plakous::sessions::AgentSessionsErrorKind::NotFound =>
                {
                    continue;
                }
                Err(error) => return Err(failure(error)),
            },
        };
        match executor
            .block_on(sessions.finish_move(operation, digest, MoveDisposition::Abort))
            .map_err(failure)?
        {
            Ok(()) => {}
            Err(error) if error.kind() == plakous::sessions::AgentSessionsErrorKind::NotFound => {}
            Err(error) => return Err(failure(error)),
        }
    }
    Ok(())
}
