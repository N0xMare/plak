use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, atomic::Ordering, mpsc};

use plakous_host_protocol::{
    views::ProfileDifference,
    wire::{LiveWork, NodeAddress, RunnerStatus, SessionResidency, SessionRow},
};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::runtime::{ResumeAfterLoss, SurfaceSession};
use plakous_recursive_subagents::tree::ProductAgentTree;
use plakous_recursive_subagents_store::{SqliteTreeStore, TreeInspection};
use tokio::sync::watch;

use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;
use crate::store::ProductStore;
use crate::store::directory::MAX_DIRECTORY_ROWS;
use crate::store::directory_notice::local::{Inbox, Notice, TreeFacts};
use crate::store::directory_projection::DirectoryMetadata;

use super::{Holder, activity_of, environment_of, runner_views};

type Rows = BTreeMap<String, Summary>;

#[derive(PartialEq, Eq)]
struct Summary {
    row: SessionRow,
    identity: String,
    degraded: bool,
}

pub(super) struct Directory {
    inbox: Arc<Inbox>,
    rows: Arc<RwLock<Rows>>,
    changes: watch::Sender<u64>,
    external: Option<External>,
}

impl Directory {
    pub(super) fn start(paths: &PlakPaths, tree: Arc<SqliteTreeStore>) -> CliResult<Self> {
        let (inbox, events) = Inbox::register(paths);
        let (changes, _) = watch::channel(0_u64);
        let mut source = Source {
            paths: paths.clone(),
            tree,
            store: ProductStore::open(paths).map_err(|error| CliFailure::new(error.to_string()))?,
            metadata: BTreeMap::new(),
            revisions: BTreeMap::new(),
            trees: BTreeMap::new(),
            profiles: BTreeMap::new(),
            withheld: BTreeSet::new(),
            omitted: BTreeSet::new(),
            holders: BTreeMap::new(),
        };
        let rows = Arc::new(RwLock::new(source.rebuild().map_err(CliFailure::new)?));
        let shared = Arc::clone(&rows);
        let gap = inbox.gap.clone();
        let notify = changes.clone();
        std::thread::Builder::new()
            .name("plakd-directory".into())
            .spawn(move || {
                while let Some(mut batch) = source.next_batch(&events) {
                    let reset = gap.swap(false, Ordering::AcqRel) || batch.gap;
                    if reset {
                        if let Ok(next) = source.rebuild() {
                            *shared.write().unwrap_or_else(PoisonError::into_inner) = next;
                            bump(&notify);
                        } else {
                            // Retry on the next notice, not on a timer.
                            gap.store(true, Ordering::Release);
                        }
                    } else {
                        // Live child state is overlaid separately from the
                        // durable roster/wake projection.
                        let tree_changed = !batch.trees.is_empty() || !batch.custody.is_empty();
                        let updates = source.apply(&mut batch);
                        let mut rows = shared.write().unwrap_or_else(PoisonError::into_inner);
                        let mut published = false;
                        for (id, summary) in updates {
                            if let Some(summary) = summary {
                                if rows.get(&id) != Some(&summary) {
                                    rows.insert(id, summary);
                                    published = true;
                                }
                            } else {
                                published |= rows.remove(&id).is_some();
                            }
                        }
                        drop(rows);
                        if published || tree_changed {
                            bump(&notify);
                        }
                    }
                    for barrier in batch.barriers {
                        let _ = barrier.send(());
                    }
                }
            })
            .map_err(|error| {
                CliFailure::new(format!("could not start session directory: {error}"))
            })?;
        Ok(Self {
            inbox,
            rows,
            changes,
            external: None,
        })
    }

    pub(super) fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(super) fn serve(
        &mut self,
        paths: &PlakPaths,
        runtime: &tokio::runtime::Handle,
        mut runner: watch::Receiver<u64>,
    ) -> CliResult<()> {
        let path = crate::store::directory_notice::socket(paths);
        External::remove_socket(&path)?;
        let entered = runtime.enter();
        let socket = tokio::net::UnixDatagram::bind(&path).map_err(|error| {
            CliFailure::new(format!("could not bind session directory notices: {error}"))
        })?;
        drop(entered);
        let inbox = Arc::downgrade(&self.inbox);
        let notify = self.changes.clone();
        let input = runtime.spawn(async move {
            let mut bytes = [0; 512];
            loop {
                let result = socket.recv(&mut bytes).await;
                let Some(inbox) = inbox.upgrade() else {
                    return;
                };
                let Ok(count) = result else {
                    inbox.send(Notice::Gap);
                    return;
                };
                inbox.send(external_notice(&bytes[..count]));
            }
        });
        let readiness = runtime.spawn(async move {
            while runner.changed().await.is_ok() {
                bump(&notify);
            }
        });
        self.external = Some(External {
            path,
            tasks: [input, readiness],
        });
        self.inbox.send(Notice::Gap);
        self.inbox.settle();
        Ok(())
    }

    pub(super) fn rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
        runners: &[runner_views::RunnerRow],
    ) -> Vec<SessionRow> {
        let by_remote_path = runner.is_some_and(|runner| runner != "local");
        let local_identity = workspace
            .filter(|_| !by_remote_path)
            .map(|workspace| crate::store::sessions::workspace_identity(Path::new(workspace)));
        let rows = self.rows.read().unwrap_or_else(PoisonError::into_inner);
        let mut selected = rows
            .values()
            .filter(|summary| {
                let row = &summary.row;
                (include_archived || !row.archived)
                    && runner.is_none_or(|runner| row.runner.runner_id == runner)
                    && if by_remote_path {
                        workspace.is_none_or(|workspace| row.workspace == workspace)
                    } else {
                        local_identity
                            .as_ref()
                            .is_none_or(|local| summary.identity == local.to_string_lossy())
                    }
            })
            .collect::<Vec<_>>();
        selected.sort_unstable_by(|left, right| {
            right
                .row
                .updated_at
                .cmp(&left.row.updated_at)
                .then_with(|| left.row.session.cmp(&right.row.session))
        });
        selected
            .into_iter()
            .take(MAX_DIRECTORY_ROWS as usize)
            .map(|summary| {
                let mut row = summary.row.clone();
                let runner = &row.runner.runner_id;
                row.runner_status = if summary.degraded {
                    RunnerStatus::Unknown
                } else {
                    runner_views::status(runner, runners).unwrap_or(RunnerStatus::Unknown)
                };
                row.runner = runner_views::label(runner, runners);
                row
            })
            .collect()
    }

    pub(super) fn settle(&self) {
        self.inbox.settle();
    }

    #[cfg(test)]
    pub(super) fn gap(&self) {
        self.inbox.send(Notice::Gap);
        self.settle();
    }
}

fn bump(notify: &watch::Sender<u64>) {
    notify.send_modify(|revision| *revision = revision.wrapping_add(1));
}

struct External {
    path: PathBuf,
    tasks: [tokio::task::JoinHandle<()>; 2],
}

impl External {
    fn remove_socket(path: &Path) -> CliResult<()> {
        use std::os::unix::fs::FileTypeExt as _;
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                std::fs::remove_file(path).map_err(|error| CliFailure::new(error.to_string()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(CliFailure::new(
                "session directory notice path is not a socket",
            )),
        }
    }
}

impl Drop for External {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = Self::remove_socket(&self.path);
    }
}

fn external_notice(bytes: &[u8]) -> Notice {
    let Some((kind, id)) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|line| line.split_once(' '))
    else {
        return Notice::Gap;
    };
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Notice::Gap;
    }
    match kind {
        "record" => Notice::Record(id.into()),
        "tree" => Notice::InspectTree(id.into()),
        "config" => Notice::Configuration,
        "visibility" => Notice::Visibility,
        _ => Notice::Gap,
    }
}

#[derive(Default)]
struct Batch {
    records: BTreeSet<String>,
    trees: BTreeMap<String, TreeUpdate>,
    custody: BTreeSet<String>,
    gap: bool,
    configuration: bool,
    visibility: bool,
    barriers: Vec<mpsc::SyncSender<()>>,
}

enum TreeUpdate {
    Image(Option<Box<TreeFacts>>),
    Inspect,
}

impl Batch {
    fn add(&mut self, notice: Notice) {
        match notice {
            Notice::Record(id) => {
                self.records.insert(id);
            }
            Notice::Tree(id, facts) => {
                self.trees.insert(id, TreeUpdate::Image(facts));
            }
            Notice::InspectTree(id) => {
                self.trees.insert(id, TreeUpdate::Inspect);
            }
            Notice::Custody(id) => {
                self.custody.insert(id);
            }
            Notice::Configuration => self.configuration = true,
            Notice::Visibility => self.visibility = true,
            Notice::Gap => self.gap = true,
            Notice::Barrier(sender) => self.barriers.push(sender),
        }
    }
}

struct Profiles {
    config: ProductConfig,
    catalog: Vec<(
        crate::surface_protocol::ProfileRow,
        crate::profiles::Prepared,
    )>,
}

struct Source {
    paths: PlakPaths,
    tree: Arc<SqliteTreeStore>,
    store: ProductStore,
    metadata: BTreeMap<String, DirectoryMetadata>,
    revisions: BTreeMap<String, u64>,
    trees: BTreeMap<String, Result<Option<Box<TreeFacts>>, ()>>,
    profiles: BTreeMap<Option<PathBuf>, Option<Profiles>>,
    withheld: BTreeSet<String>,
    omitted: BTreeSet<String>,
    holders: BTreeMap<String, Holder>,
}

impl Source {
    fn next_batch(&self, events: &mpsc::Receiver<Notice>) -> Option<Batch> {
        loop {
            let event = if self.holders.is_empty() {
                events.recv().ok()?
            } else {
                match events.recv_timeout(std::time::Duration::from_millis(250)) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // A dead process cannot send a release notice. Probe only
                        // already observed holders' locks, not the directory stores.
                        let records = self
                            .holders
                            .iter()
                            .filter(|(_, holder)| holder.gone(&self.paths, &self.tree))
                            .map(|(id, _)| id.clone())
                            .collect::<BTreeSet<_>>();
                        if records.is_empty() {
                            continue;
                        }
                        return Some(Batch {
                            records,
                            ..Batch::default()
                        });
                    }
                }
            };
            let mut batch = Batch::default();
            for event in std::iter::once(event).chain(events.try_iter().take(255)) {
                batch.add(event);
            }
            return Some(batch);
        }
    }

    fn apply(&mut self, batch: &mut Batch) -> Vec<(String, Option<Summary>)> {
        let mut affected = std::mem::take(&mut batch.custody);
        if batch.configuration {
            self.profiles.clear();
            affected.extend(self.metadata.values().map(|row| row.node.tree_id.clone()));
        }
        if batch.visibility
            && let Ok(withheld) = crate::carry::upgrade::receipt::withheld_targets(&self.paths)
        {
            for id in withheld.symmetric_difference(&self.withheld) {
                if let Some(row) = self.metadata.get(id) {
                    affected.insert(row.node.tree_id.clone());
                }
            }
            self.withheld = withheld;
        }
        self.update(
            std::mem::take(&mut batch.records),
            std::mem::take(&mut batch.trees),
            affected,
        )
    }

    fn rebuild(&mut self) -> Result<Rows, String> {
        let ids = self.store.session_ids()?.into_iter().collect();
        self.withheld = crate::carry::upgrade::receipt::withheld_targets(&self.paths)
            .map_err(|error| error.message().to_owned())?;
        self.metadata.clear();
        self.revisions.clear();
        self.trees.clear();
        self.profiles.clear();
        self.holders.clear();
        Ok(self
            .update(ids, BTreeMap::new(), BTreeSet::new())
            .into_iter()
            .filter_map(|(id, summary)| summary.map(|summary| (id, summary)))
            .collect())
    }

    fn update(
        &mut self,
        ids: BTreeSet<String>,
        trees: BTreeMap<String, TreeUpdate>,
        mut affected: BTreeSet<String>,
    ) -> Vec<(String, Option<Summary>)> {
        for (id, update) in trees {
            let facts = match update {
                TreeUpdate::Image(facts) => Ok(facts),
                TreeUpdate::Inspect => inspect_tree(&self.paths, &id),
            };
            if self.trees.get(&id) != Some(&facts) {
                affected.insert(id.clone());
                self.trees.insert(id, facts);
            }
        }
        let mut changes = Vec::new();
        let mut locators = Vec::new();
        for id in ids {
            self.holders.remove(&id);
            self.revisions.remove(&id);
            if let Some(previous) = self.metadata.remove(&id) {
                affected.insert(previous.node.tree_id);
            }
            match self.store.directory_metadata(&id) {
                Ok(Some(metadata)) => {
                    affected.insert(metadata.node.tree_id.clone());
                    if let Some(locator) = metadata.locator {
                        locators.push((id.clone(), locator));
                    }
                    self.metadata.insert(id, metadata);
                }
                Ok(None) => changes.push((id, None)),
                Err(_) => {
                    self.omit(&id, "metadata");
                    changes.push((id, None));
                }
            }
        }
        self.refresh_revisions(locators);
        changes.extend(self.project(&affected));
        changes
    }

    fn refresh_revisions(&mut self, located: Vec<(String, plakous::sessions::SessionLocator)>) {
        if located.is_empty() {
            return;
        }
        let locators = located
            .iter()
            .map(|(_, locator)| *locator)
            .collect::<Vec<_>>();
        if let Ok(revisions) = plakous::sessions::AgentSessions::inspect_branch_revisions_sqlite(
            self.paths.runtime_database(),
            &locators,
        ) {
            self.revisions.extend(
                located
                    .into_iter()
                    .zip(revisions)
                    .filter_map(|((id, _), revision)| revision.map(|revision| (id, revision))),
            );
        }
    }

    fn project(&mut self, affected: &BTreeSet<String>) -> Vec<(String, Option<Summary>)> {
        let entries = self
            .metadata
            .iter()
            .filter(|(_, meta)| affected.contains(&meta.node.tree_id))
            .collect::<Vec<_>>();
        let mut heads = BTreeMap::new();
        let mut updates = Vec::with_capacity(entries.len());
        for (id, meta) in entries {
            let revision = self.revisions.get(id).copied().unwrap_or(0);
            let tree_id = &meta.node.tree_id;
            let head = heads
                .entry(tree_id)
                .or_insert_with(|| self.store.head_of_tree(tree_id).ok().flatten());
            let tree = self
                .trees
                .entry(tree_id.clone())
                .or_insert_with(|| inspect_tree(&self.paths, tree_id));
            let profile_key = meta
                .runner
                .as_ref()
                .is_none_or(|runner| runner.runner_id == "local")
                .then(|| meta.workspace.clone());
            let profiles = self
                .profiles
                .entry(profile_key.clone())
                .or_insert_with(|| load_profiles(&self.paths, profile_key.as_deref()));
            let summary = (!self.withheld.contains(id))
                .then(|| {
                    summarize(
                        &self.paths,
                        &self.tree,
                        &mut self.store,
                        id,
                        meta,
                        Projection {
                            head: head.as_deref() == Some(id.as_str()),
                            tree: tree.as_ref().ok().and_then(|facts| facts.as_deref()),
                            degraded_tree: tree.is_err(),
                            profiles: profiles.as_ref(),
                            revision,
                        },
                    )
                })
                .transpose();
            self.holders.remove(id);
            let summary = match summary {
                Ok(Some((summary, holder))) => {
                    if let Some(holder) = holder {
                        self.holders.insert(id.clone(), holder);
                    }
                    Some(summary)
                }
                Ok(None) => None,
                Err(reason) => {
                    diagnostic(&mut self.omitted, id, reason);
                    None
                }
            };
            updates.push((id.clone(), summary));
        }
        updates
    }

    fn omit(&mut self, id: &str, reason: &str) {
        diagnostic(&mut self.omitted, id, reason);
    }
}

fn diagnostic(omitted: &mut BTreeSet<String>, id: &str, reason: &str) {
    if omitted.insert(id.to_owned()) {
        eprintln!("plakd host: session directory row omitted ({reason})");
    }
}

fn inspect_tree(paths: &PlakPaths, tree: &str) -> Result<Option<Box<TreeFacts>>, ()> {
    match SqliteTreeStore::inspect(&paths.subagents_database(), tree).map_err(|_| ())? {
        TreeInspection::Tree(rows) => Ok(Some(Box::new(TreeFacts::of(&ProductAgentTree::reopen(
            *rows,
        ))))),
        TreeInspection::StoreAbsent | TreeInspection::TreeAbsent => Ok(None),
    }
}

fn load_profiles(paths: &PlakPaths, workspace: Option<&Path>) -> Option<Profiles> {
    let config = match workspace {
        Some(workspace) => ProductConfig::load(&paths.for_workspace(workspace)),
        None => ProductConfig::load_user(paths),
    }
    .ok()?;
    let catalog = crate::profiles::prepared_catalog(&config, paths.home());
    Some(Profiles { config, catalog })
}

struct Projection<'a> {
    head: bool,
    tree: Option<&'a TreeFacts>,
    degraded_tree: bool,
    profiles: Option<&'a Profiles>,
    revision: u64,
}

fn summarize(
    paths: &PlakPaths,
    tree: &SqliteTreeStore,
    store: &mut ProductStore,
    id: &str,
    meta: &DirectoryMetadata,
    projection: Projection<'_>,
) -> Result<(Summary, Option<Holder>), &'static str> {
    let profiles = projection.profiles;
    let policy = profiles.map_or(ResumeAfterLoss::Hold, |profiles| {
        crate::session_attention::policy(&profiles.config)
    });
    let (activity, root, wakes) = store
        .directory_activity(id, policy)
        .map_err(|_| "activity")?;
    let holder =
        super::holder(paths, tree, &meta.node.tree_id, meta.locator).map_err(|_| "holder")?;
    let hosted_elsewhere = holder.as_ref().map(Holder::wire);
    let tree = projection.tree;
    let work = tree.map_or_default(|tree| match policy {
        ResumeAfterLoss::Hold => tree.hold,
        ResumeAfterLoss::Continue => tree.resume,
    });
    let needs_operator = projection.head
        && !meta.archived
        && profiles.is_some()
        && work.attention(root, 0, wakes).is_some();
    let (profile, differences) = profile(id, meta, profiles.zip(tree));
    Ok((
        Summary {
            identity: meta.identity.clone(),
            degraded: profiles.is_none() || projection.degraded_tree,
            row: SessionRow {
                runner: runner_views::label(
                    meta.runner
                        .as_ref()
                        .map_or("local", |runner| runner.runner_id.as_str()),
                    &[],
                ),
                runner_status: RunnerStatus::Unknown,
                needs_operator,
                activity_seq: projection.revision,
                archived: meta.archived,
                differences,
                environment: environment_of(hosted_elsewhere.is_some()),
                hosted_elsewhere,
                node: NodeAddress {
                    tree_id: meta.node.tree_id.clone(),
                    agent_id: meta.node.agent_id.clone(),
                },
                title: meta.title.clone(),
                workspace: meta.workspace.display().to_string(),
                profile: Some(profile),
                created_at: meta.created_at,
                updated_at: meta.updated_at,
                residency: SessionResidency::Dormant,
                activity: activity_of(activity),
                live_work: LiveWork::default(),
                session: id.to_owned(),
            },
        },
        holder,
    ))
}

fn profile(
    id: &str,
    meta: &DirectoryMetadata,
    input: Option<(&Profiles, &TreeFacts)>,
) -> (String, Vec<ProfileDifference>) {
    let Some((profiles, tree)) = input else {
        return ("custom".into(), vec![ProfileDifference::Roster]);
    };
    let profile = crate::shell::settings::session_profile_with_profiles(
        &profiles.config,
        &SurfaceSession {
            runner: meta.runner.clone(),
            id: id.to_owned(),
            account: meta.account.clone(),
            model: meta.model.clone(),
            effort: meta.effort.clone(),
            fast: meta.fast,
            workspace: meta.workspace.clone(),
            roster: Some(tree.roster.clone()),
            live_children: false,
            mcp: None,
            account_choices: BTreeMap::new(),
        },
        &profiles.catalog,
    );
    (profile.id, profile.differences)
}
