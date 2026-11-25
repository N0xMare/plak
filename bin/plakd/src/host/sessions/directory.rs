use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use plakous_host_protocol::{
    views::ProfileDifference,
    wire::{LiveWork, NodeAddress, RunnerStatus, SessionResidency, SessionRow},
};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::runtime::{ResumeAfterLoss, SurfaceSession};
use plakous_recursive_subagents_store::{SqliteTreeStore, TreeDirectoryProbe, TreeInspection};
use tokio::sync::watch;

use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;
use crate::store::ProductStore;
use crate::store::directory::MAX_DIRECTORY_ROWS;
use crate::store::directory_notice::local::{Batch, Inbox, Mailbox, Notice, TreeFacts, TreeUpdate};
use crate::store::directory_projection::{DirectoryMetadata, ProductDirectoryProbe};

use super::{Holder, activity_of, environment_of, runner_views};

mod draft;
mod files;

const REVALIDATE: Duration = Duration::from_secs(15);
const PROBE_HOLDERS: Duration = Duration::from_millis(250);

type Rows = BTreeMap<String, Summary>;

#[derive(Clone, PartialEq, Eq)]
struct Summary {
    row: SessionRow,
    identity: String,
    degraded: bool,
    draft: Option<draft::Facts>,
}

pub(super) struct Directory {
    inbox: Arc<Inbox>,
    rows: Arc<RwLock<Rows>>,
    changes: watch::Sender<u64>,
    external: Option<External>,
    draft_attention: draft::Stamps,
    #[cfg(test)]
    pub(super) runtime_inspections: Arc<std::sync::atomic::AtomicUsize>,
}

impl Directory {
    pub(super) fn start(paths: &PlakPaths, tree: Arc<SqliteTreeStore>) -> CliResult<Self> {
        let (inbox, events) = Inbox::register(paths);
        let (changes, _) = watch::channel(0_u64);
        let mut source = Source {
            paths: paths.clone(),
            tree,
            store: ProductStore::open(paths).map_err(|error| CliFailure::new(error.to_string()))?,
            product_probe: ProductDirectoryProbe::open(paths).map_err(CliFailure::new)?,
            tree_probe: TreeDirectoryProbe::open(&paths.subagents_database())
                .map_err(|error| CliFailure::new(error.message()))?,
            files: files::Inputs::new(paths),
            metadata: BTreeMap::new(),
            revisions: BTreeMap::new(),
            retry_revisions: BTreeSet::new(),
            trees: BTreeMap::new(),
            profiles: BTreeMap::new(),
            omitted: BTreeSet::new(),
            retry_rows: BTreeSet::new(),
            holders: BTreeMap::new(),
            draft: Some(draft::Source::new(paths)),
            #[cfg(test)]
            runtime_inspections: Arc::default(),
        };
        let rows = Arc::new(RwLock::new(source.rebuild().map_err(CliFailure::new)?));
        #[cfg(test)]
        let runtime_inspections = Arc::clone(&source.runtime_inspections);
        let shared = Arc::clone(&rows);
        let notify = changes.clone();
        std::thread::Builder::new()
            .name("plakd-directory".into())
            .spawn(move || source.run(&events, &shared, &notify))
            .map_err(|error| {
                CliFailure::new(format!("could not start session directory: {error}"))
            })?;
        Ok(Self {
            inbox,
            rows,
            changes,
            external: None,
            draft_attention: draft::Stamps::default(),
            #[cfg(test)]
            runtime_inspections,
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
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            let _ = External::remove_socket(&path);
            return Err(CliFailure::new(format!(
                "could not protect session directory notices: {error}"
            )));
        }
        drop(entered);
        let inbox = Arc::downgrade(&self.inbox);
        let notify = self.changes.clone();
        let input = runtime.spawn(async move {
            let mut bytes = [0; 512];
            let mut receive_failed = false;
            loop {
                let result = socket.recv(&mut bytes).await;
                let Some(recipient) = inbox.upgrade() else {
                    return;
                };
                if let Ok(count) = result {
                    receive_failed = false;
                    if let Some(notice) = external_notice(&bytes[..count]) {
                        recipient.send_external(notice);
                    }
                } else {
                    // A receive interruption can hide changed keys. Request
                    // one rebuild per interruption, not one on every retry.
                    if !std::mem::replace(&mut receive_failed, true) {
                        recipient.send_external(Notice::Gap);
                    }
                    drop(recipient);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
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
        self.inbox.send(Notice::Revalidate);
        Ok(())
    }

    pub(super) fn rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
        runners: &[runner_views::RunnerRow],
    ) -> Vec<SessionRow> {
        self.select(workspace, runner, include_archived, |summary| {
            let mut row = summary.row.clone();
            let runner = &row.runner.runner_id;
            row.runner_status = runner_status(summary, runners);
            row.runner = runner_views::label(runner, runners);
            Some(row)
        })
    }

    fn select<T>(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
        project: impl FnMut(&Summary) -> Option<T>,
    ) -> Vec<T> {
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
            .filter_map(project)
            .collect()
    }

    #[cfg(test)]
    pub(super) fn settle(&self) {
        self.inbox.settle();
    }

    #[cfg(test)]
    pub(super) fn gap(&self) {
        self.inbox.send(Notice::Gap);
        self.settle();
    }

    #[cfg(test)]
    pub(super) fn tick(&self) {
        self.inbox.send(Notice::Revalidate);
        self.settle();
    }
}

fn runner_status(summary: &Summary, runners: &[runner_views::RunnerRow]) -> RunnerStatus {
    if summary.degraded {
        RunnerStatus::Unknown
    } else {
        runner_views::status(&summary.row.runner.runner_id, runners)
            .unwrap_or(RunnerStatus::Unknown)
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

fn external_notice(bytes: &[u8]) -> Option<Notice> {
    let (kind, id) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|line| line.split_once(' '))?;
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    match kind {
        "record" => Some(Notice::Record(id.into())),
        "tree" => Some(Notice::InspectTree(id.into())),
        "custody" => Some(Notice::Custody(id.into())),
        "config" => Some(Notice::Configuration),
        _ => None,
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
    product_probe: ProductDirectoryProbe,
    tree_probe: TreeDirectoryProbe,
    files: files::Inputs,
    metadata: BTreeMap<String, DirectoryMetadata>,
    revisions: BTreeMap<String, u64>,
    retry_revisions: BTreeSet<String>,
    trees: BTreeMap<String, Result<Option<Box<TreeFacts>>, ()>>,
    profiles: BTreeMap<Option<PathBuf>, Option<Profiles>>,
    omitted: BTreeSet<String>,
    retry_rows: BTreeSet<String>,
    holders: BTreeMap<String, Holder>,
    draft: Option<draft::Source>,
    #[cfg(test)]
    runtime_inspections: Arc<std::sync::atomic::AtomicUsize>,
}

impl Source {
    fn run(&mut self, events: &Mailbox, shared: &RwLock<Rows>, notify: &watch::Sender<u64>) {
        let interval = crate::test_hooks::host_lifecycle::directory_revalidation(REVALIDATE);
        let mut next_tick = Instant::now() + interval;
        let mut next_probe = Instant::now() + PROBE_HOLDERS;
        let mut retry_rebuild = false;
        while let Some(mut batch) = events.take_until(if self.holders.is_empty() {
            next_tick
        } else {
            next_tick.min(next_probe)
        }) {
            let now = Instant::now();
            if now >= next_probe {
                next_probe = now + PROBE_HOLDERS;
                batch.records.extend(
                    self.holders
                        .iter()
                        .filter(|(_, holder)| holder.gone(&self.paths, &self.tree))
                        .map(|(id, _)| id.clone()),
                );
            }
            let tick = now >= next_tick || batch.revalidate;
            if now >= next_tick {
                next_tick = now + interval;
            }
            if batch.gap || (retry_rebuild && tick) {
                retry_rebuild = match self.rebuild() {
                    Ok(next) => {
                        *shared.write().unwrap_or_else(PoisonError::into_inner) = next;
                        bump(notify);
                        false
                    }
                    Err(_) => true,
                };
            }
            if tick {
                self.revalidate(&mut batch);
            }
            // Live child state is overlaid separately.
            let tree_changed = !batch.trees.is_empty() || !batch.custody.is_empty();
            let costs_changed = tick
                && self
                    .draft
                    .as_mut()
                    .is_some_and(|draft| draft.costs.poll(&self.paths));
            let mut updates = self.apply(&mut batch);
            if costs_changed {
                self.refresh_draft_costs(shared, &mut updates);
            }
            if publish(shared, updates) || tree_changed {
                bump(notify);
            }
            #[cfg(test)]
            for barrier in batch.barriers {
                let _ = barrier.send(());
            }
        }
    }

    fn revalidate(&mut self, batch: &mut Batch) {
        if let Some(draft) = &self.draft {
            batch.records.extend(
                self.metadata
                    .keys()
                    .filter(|id| !draft.previews.contains_key(*id))
                    .cloned(),
            );
        }
        if let Ok(records) = self.product_probe.changes() {
            batch.records.extend(records);
        }
        if let Ok(trees) = self.tree_probe.changes() {
            for id in trees.images {
                batch.trees.entry(id).or_insert(TreeUpdate::Inspect);
            }
            batch.custody.extend(trees.custody);
        }
        batch
            .records
            .extend(self.retry_rows.iter().chain(&self.retry_revisions).cloned());
        for (id, facts) in &self.trees {
            if facts.is_err() {
                batch.trees.entry(id.clone()).or_insert(TreeUpdate::Inspect);
            }
        }
        batch.configuration |= self.files.global_changed(&self.paths);
        self.profiles.retain(|workspace, profile| {
            if profile.is_some()
                && !self
                    .files
                    .workspace_changed(&self.paths, workspace.as_deref())
            {
                return true;
            }
            batch.custody.extend(
                self.metadata
                    .values()
                    .filter(|meta| profile_workspace(meta) == workspace.as_deref())
                    .map(|meta| meta.node.tree_id.clone()),
            );
            false
        });
    }

    fn refresh_draft_costs(
        &mut self,
        shared: &RwLock<Rows>,
        updates: &mut Vec<(String, Option<Summary>)>,
    ) {
        let Some(draft) = &mut self.draft else { return };
        let affected = updates
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<BTreeSet<_>>();
        let rows = shared.read().unwrap_or_else(PoisonError::into_inner);
        let measured = rows
            .iter()
            .filter_map(|(id, summary)| {
                if affected.contains(id.as_str()) {
                    return None;
                }
                let tree = self
                    .trees
                    .get(&summary.row.node.tree_id)
                    .and_then(|facts| facts.as_ref().ok())
                    .and_then(|facts| facts.as_deref());
                let facts = summary.draft.as_ref()?.measure(&mut draft.costs, tree)?;
                let mut summary = summary.clone();
                summary.draft = Some(facts);
                Some((id.clone(), Some(summary)))
            })
            .collect::<Vec<_>>();
        updates.extend(measured);
    }

    fn apply(&mut self, batch: &mut Batch) -> Vec<(String, Option<Summary>)> {
        let mut affected = std::mem::take(&mut batch.custody);
        if batch.configuration {
            self.profiles.clear();
            affected.extend(self.metadata.values().map(|row| row.node.tree_id.clone()));
        }
        self.update(
            std::mem::take(&mut batch.records),
            std::mem::take(&mut batch.trees),
            affected,
        )
    }

    fn rebuild(&mut self) -> Result<Rows, String> {
        let ids = self.store.session_ids()?.into_iter().collect();
        self.metadata.clear();
        self.revisions.clear();
        self.retry_revisions.clear();
        self.trees.clear();
        self.profiles.clear();
        self.holders.clear();
        self.retry_rows.clear();
        if let Some(draft) = &mut self.draft {
            draft.previews.clear();
        }
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
            // Actors can acknowledge a save without advancing its revision
            // or changing any directory facts. Do not query either store
            // for a presentation image the directory already has.
            if let TreeUpdate::Image(facts) = &update
                && matches!(self.trees.get(&id), Some(Ok(previous)) if previous == facts)
            {
                continue;
            }
            let facts = match update {
                TreeUpdate::Image(facts)
                    if self.tree_probe.revision(&id).is_ok_and(|current| {
                        current == facts.as_ref().map(|facts| facts.revision)
                    }) =>
                {
                    Ok(facts)
                }
                TreeUpdate::Image(_) | TreeUpdate::Inspect => inspect_tree(&self.paths, &id),
            };
            if self.trees.get(&id) == Some(&facts) {
                continue;
            }
            if let Ok(facts) = &facts {
                self.tree_probe
                    .note_image(&id, facts.as_ref().map(|facts| facts.revision));
            }
            if !matches!(
                (self.trees.get(&id), &facts),
                (Some(Ok(Some(previous))), Ok(Some(current))) if previous.same_projection(current)
                    && (self.draft.is_none() || previous.same_draft_projection(current))
            ) {
                affected.insert(id.clone());
            }
            self.trees.insert(id, facts);
        }
        let mut changes = Vec::new();
        let mut locators = Vec::new();
        for id in ids {
            self.holders.remove(&id);
            self.revisions.remove(&id);
            self.retry_rows.remove(&id);
            self.retry_revisions.remove(&id);
            if let Some(draft) = &mut self.draft {
                draft.previews.remove(&id);
            }
            if let Some(previous) = self.metadata.remove(&id) {
                affected.insert(previous.node.tree_id);
            }
            match self.store.directory_metadata(&id) {
                Ok(Some(metadata)) => {
                    if let Some(draft) = &mut self.draft
                        && let Ok(text) = self.store.directory_preview(&id)
                    {
                        draft
                            .previews
                            .insert(id.clone(), text.and_then(draft::preview));
                    }
                    self.product_probe.note(&id, Some(&metadata));
                    affected.insert(metadata.node.tree_id.clone());
                    if let Some(locator) = metadata.locator {
                        locators.push((id.clone(), locator));
                    }
                    self.metadata.insert(id, metadata);
                }
                Ok(None) => {
                    self.product_probe.note(&id, None);
                    changes.push((id, None));
                }
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
        #[cfg(test)]
        self.runtime_inspections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let locators = located
            .iter()
            .map(|(_, locator)| *locator)
            .collect::<Vec<_>>();
        if let Ok(revisions) = plakous::sessions::AgentSessions::inspect_branch_revisions_sqlite(
            self.paths.runtime_database(),
            &locators,
        ) {
            for ((id, _), revision) in located.into_iter().zip(revisions) {
                if let Some(revision) = revision {
                    self.retry_revisions.remove(&id);
                    self.revisions.insert(id, revision);
                }
            }
        } else {
            self.retry_revisions
                .extend(located.into_iter().map(|(id, _)| id));
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
            let profile_key = profile_workspace(meta).map(Path::to_owned);
            let profiles = self.profiles.entry(profile_key).or_insert_with_key(|key| {
                self.files.remember_workspace(&self.paths, key.as_deref());
                load_profiles(&self.paths, key.as_deref())
            });
            let summary = summarize(
                &self.paths,
                &self.tree,
                &mut self.store,
                id,
                meta,
                Projection {
                    head: head.as_deref() == Some(id.as_str()),
                    open_target: head.as_deref(),
                    tree: tree.as_ref().ok().and_then(|facts| facts.as_deref()),
                    degraded_tree: tree.is_err(),
                    profiles: profiles.as_ref(),
                    revision,
                    draft: self.draft.as_mut(),
                },
            );
            self.holders.remove(id);
            self.retry_rows.remove(id);
            let summary = match summary {
                Ok((summary, holder)) => {
                    if let Some(holder) = holder {
                        self.holders.insert(id.clone(), holder);
                    }
                    Some(summary)
                }
                Err(reason) => {
                    diagnostic(&mut self.omitted, id, reason);
                    self.retry_rows.insert(id.clone());
                    None
                }
            };
            updates.push((id.clone(), summary));
        }
        updates
    }

    fn omit(&mut self, id: &str, reason: &str) {
        diagnostic(&mut self.omitted, id, reason);
        self.retry_rows.insert(id.to_owned());
    }
}

fn profile_workspace(meta: &DirectoryMetadata) -> Option<&Path> {
    meta.runner
        .as_ref()
        .is_none_or(|runner| runner.runner_id == "local")
        .then_some(meta.workspace.as_path())
}

fn publish(shared: &RwLock<Rows>, updates: Vec<(String, Option<Summary>)>) -> bool {
    if updates.is_empty() {
        return false;
    }
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
    published
}

fn diagnostic(omitted: &mut BTreeSet<String>, id: &str, reason: &str) {
    if omitted.insert(id.to_owned()) {
        eprintln!("plakd host: session directory row omitted ({reason})");
    }
}

fn inspect_tree(paths: &PlakPaths, tree: &str) -> Result<Option<Box<TreeFacts>>, ()> {
    match SqliteTreeStore::inspect(&paths.subagents_database(), tree).map_err(|_| ())? {
        TreeInspection::Tree(rows) => Ok(Some(Box::new(TreeFacts::of(rows.rows())))),
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
    open_target: Option<&'a str>,
    tree: Option<&'a TreeFacts>,
    degraded_tree: bool,
    profiles: Option<&'a Profiles>,
    revision: u64,
    draft: Option<&'a mut draft::Source>,
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
    let activity = store
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
        && work.attention(activity.root, 0, activity.wakes).is_some();
    let (profile, differences) = profile(id, meta, profiles.zip(tree));
    let draft = projection.draft.map(|draft| {
        let tier = draft::service_tier(meta.service_tier.as_deref());
        let tier_differs = profiles
            .and_then(|profiles| profiles.catalog.iter().find(|(row, _)| row.id == profile))
            .is_some_and(|(_, prepared)| draft::service_tier(prepared.root.service_tier()) != tier);
        let remote = hosted_elsewhere.is_some()
            || meta
                .runner
                .as_ref()
                .is_some_and(|runner| runner.runner_id != "local");
        let mut facts = draft.project(
            id,
            meta,
            &activity,
            draft::Context {
                tree,
                head: projection.head,
                remote,
                policy,
            },
            draft::differences(&differences, tier, tier_differs),
        );
        if let Some(target) = projection.open_target.filter(|target| *target != id)
            && store.has_binding_successor(id).unwrap_or(false)
        {
            facts.superseded_by = Some(target.to_owned());
        }
        facts
    });
    Ok((
        Summary {
            identity: meta.identity.clone(),
            degraded: profiles.is_none() || projection.degraded_tree,
            draft,
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
                activity: activity_of(activity.standing),
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
            service_tier: meta.service_tier.clone(),
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
