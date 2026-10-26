//! In-process JJ operations. No command parsing or recovery checkout occurs here.
pub(super) const ENABLED: bool = true;

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories, default_working_copy_factory,
};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, Matcher, NothingMatcher, Visit, VisitDirs, VisitFiles};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::repo_path::RepoPath;
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::{SnapshotOptions, WorkingCopyFreshness};
use jj_lib::workspace::Workspace;
use sha2::{Digest, Sha256};

use super::occupancy_vcs::{
    AdoptFailure, AdoptLanding, ConflictPath, MintedCopy, VcsError, VcsSnapshot,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
struct RecoveryRequired(String);
impl std::fmt::Display for RecoveryRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RecoveryRequired {}
fn recovery(detail: impl Into<String>) -> Error {
    Box::new(RecoveryRequired(detail.into()))
}
fn failure(error: &Error) -> VcsError {
    let kind = if error.is::<RecoveryRequired>() {
        super::occupancy_vcs::VcsErrorKind::RecoveryRequired
    } else {
        super::occupancy_vcs::VcsErrorKind::SealFailed
    };
    VcsError::new(kind, format!("JJ operation failed: {error}"))
}

fn settings() -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(ConfigLayer::parse(ConfigSource::CommandArg, "user.name='plak'\nuser.email='plak@local'\noperation.hostname='local'\noperation.username='plak'\nsigning.behavior='drop'\n")?);
    Ok(UserSettings::from_config_and_home_dir(config, None)?)
}

fn load(path: &Path) -> Result<Workspace> {
    Ok(Workspace::load(
        &settings()?,
        path,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )?)
}

async fn current(repo: &ReadonlyRepo, ws: &Workspace) -> Result<Commit> {
    let id = repo
        .view()
        .get_wc_commit_id(ws.workspace_name())
        .ok_or("workspace no longer registered; recovery required")?;
    Ok(repo.store().get_commit_async(id).await?)
}

fn tree_content(tree: &MergedTree) -> String {
    let mut hash = Sha256::new();
    hash.update(b"plak-jj-tree-v1\0");
    for terms in [
        tree.tree_ids()
            .iter()
            .map(ObjectId::hex)
            .collect::<Vec<_>>(),
        tree.labels().as_slice().to_vec(),
    ] {
        hash.update((terms.len() as u64).to_be_bytes());
        for term in terms {
            hash.update((term.len() as u64).to_be_bytes());
            hash.update(term.as_bytes());
        }
    }
    let mut result = String::from("jj-tree-v1:");
    for byte in hash.finalize() {
        let _ = write!(result, "{byte:02x}");
    }
    result
}
#[derive(Debug)]
struct SnapshotPaths;
impl Matcher for SnapshotPaths {
    fn matches(&self, path: &RepoPath) -> bool {
        !super::occupancy_vcs::is_secret_git_path(path.as_internal_file_string())
    }
    fn visit(&self, _: &RepoPath) -> Visit {
        Visit::Specific {
            dirs: VisitDirs::All,
            files: VisitFiles::All,
        }
    }
}

fn description(commit: &Commit) -> VcsSnapshot {
    let content = tree_content(&commit.tree());
    VcsSnapshot {
        history_left_base: false,
        commit: commit.id().hex(),
        content,
        change_id: Some(commit.change_id().reverse_hex()),
    }
}

fn snapshot_options() -> Result<SnapshotOptions<'static>> {
    let ignores =
        GitIgnoreFile::empty().chain(RepoPath::root(), Path::new("plak-excludes"), b".plak/\n")?;
    Ok(SnapshotOptions {
        base_ignores: ignores,
        progress: None,
        start_tracking_matcher: &SnapshotPaths,
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size: 1024 * 1024 * 1024,
    })
}

/// Publish only if the exact operation we evaluated is still the sole head.
/// The standard store lock is also taken by external JJ 0.39/0.45 publishers.
async fn publish_checked(
    tx: jj_lib::transaction::Transaction,
    description: &str,
) -> Result<Arc<ReadonlyRepo>> {
    let base = tx.base_repo().op_id().clone();
    let heads = tx.base_repo().op_heads_store().clone();
    if heads.name() != jj_lib::simple_op_heads_store::SimpleOpHeadsStore::NAME {
        return Err("only the locking standard JJ operation-head store is supported".into());
    }
    let prepared = tx.write(description).await?;
    let lock = heads.lock().await?;
    if heads.get_op_heads().await? != [base.clone()] {
        prepared.leave_unpublished();
        return Err(recovery(
            "repository advanced before publication; no checkout attempted; recovery required",
        ));
    }
    let result = heads
        .update_op_heads(&[base], prepared.operation().id())
        .await;
    let repo = prepared.leave_unpublished();
    drop(lock);
    result.map_err(|_| recovery("repository publication outcome uncertain; recovery required"))?;
    Ok(repo)
}

/// Import an externally moved Git HEAD without resetting the Git index or
/// checking out files. The actual working files are snapshotted by the caller.
async fn import_git_head(ws: &mut Workspace) -> Result<()> {
    if !ws.workspace_root().join(".git").exists() {
        return Ok(());
    }
    let repo = ws.repo_loader().load_at_head().await?;
    let name = ws.workspace_name().to_owned();
    let root = ws.workspace_root().to_owned();
    let mut tx = repo.start_transaction();
    jj_lib::git::import_head(tx.repo_mut(), &name, &root).await?;
    let previous = repo.view().git_head(&name);
    let imported = tx.repo().git_head(&name);
    if previous == imported {
        return Ok(());
    }
    let new_head = imported
        .as_normal()
        .ok_or_else(|| recovery("Git HEAD became unborn or ambiguous; recovery required"))?
        .clone();
    let old = current(&repo, ws).await?;
    let mut locked = ws.start_working_copy_mutation().await?;
    if WorkingCopyFreshness::check_stale(locked.locked_wc(), &old, &repo).await?
        != WorkingCopyFreshness::Fresh
    {
        return Err(recovery(
            "Git HEAD and JJ working copy both changed; files preserved; recovery required",
        ));
    }
    let head = repo.store().get_commit_async(&new_head).await?;
    let wc = tx.repo_mut().check_out(name, &head).await?;
    locked.locked_wc().reset(&wc).await?;
    tx.repo_mut().rebase_descendants().await?;
    let repo = publish_checked(tx, "plak import Git HEAD")
        .await
        .map_err(|_| recovery("Git HEAD import publication uncertain; recovery required"))?;
    locked.finish(repo.op_id().clone()).await.map_err(|_| {
        recovery("Git HEAD imported but working-copy state not saved; recovery required")
    })?;
    Ok(())
}

/// Snapshot only a working copy whose on-disk tree agrees with the current
/// repository view. A rewritten stale copy is left untouched, including files
/// which have never been snapshotted. Recovery is an explicit operator action.
async fn snapshot_workspace(ws: &mut Workspace) -> Result<(Arc<ReadonlyRepo>, Commit)> {
    import_git_head(ws).await?;
    let repo = ws.repo_loader().load_at_head().await?;
    let commit = current(&repo, ws).await?;
    let name = ws.workspace_name().to_owned();
    let mut locked = ws.start_working_copy_mutation().await?;
    if WorkingCopyFreshness::check_stale(locked.locked_wc(), &commit, &repo).await?
        != WorkingCopyFreshness::Fresh
    {
        return Err(recovery(
            "working copy recovery required: repository history changed; files were preserved and no checkout was attempted",
        ));
    }
    let options = snapshot_options()?;
    let (tree, stats) = locked.locked_wc().snapshot(&options).await?;
    if !stats.invalid_utf8_paths.is_empty()
        || stats.untracked_paths.values().any(|reason| {
            matches!(
                reason,
                jj_lib::working_copy::UntrackedReason::FileTooLarge { .. }
            )
        })
    {
        return Err(
            "snapshot incomplete: unsupported or oversized untracked files; files preserved".into(),
        );
    }
    let previous_tree = commit.tree();
    let mut changes = previous_tree.diff_stream(&tree, &EverythingMatcher);
    while let Some(entry) = changes.next().await {
        entry.values?;
        if super::occupancy_vcs::is_secret_git_path(entry.path.as_internal_file_string()) {
            return Err(
                "snapshot refused changes to a tracked secret path; working files preserved".into(),
            );
        }
    }
    if tree.tree_ids_and_labels() == commit.tree().tree_ids_and_labels() {
        locked.finish(repo.op_id().clone()).await?;
        return Ok((repo, commit));
    }
    let mut tx = repo.start_transaction();
    let updated = tx
        .repo_mut()
        .rewrite_commit(&commit)
        .set_tree(tree)
        .write()
        .await?;
    tx.repo_mut().set_wc_commit(name, updated.id().clone())?;
    tx.repo_mut().rebase_descendants().await?;
    let repo = publish_checked(tx, "plak snapshot")
        .await
        .map_err(|_| recovery("snapshot publication uncertain; recovery required"))?;
    locked.finish(repo.op_id().clone()).await.map_err(|_| {
        recovery("snapshot published but working-copy state not saved; recovery required")
    })?;
    Ok((repo, updated))
}

pub(super) fn snapshot(path: &Path) -> std::result::Result<VcsSnapshot, VcsError> {
    pollster::block_on(async {
        let mut ws = load(path)?;
        let (_, commit) = snapshot_workspace(&mut ws).await?;
        Ok::<_, Error>(description(&commit))
    })
    .map_err(|error| failure(&error))
}

fn git_common_dir(workspace: &Workspace) -> Result<std::path::PathBuf> {
    pollster::block_on(async {
        let repo = workspace.repo_loader().load_at_head().await?;
        let backend = repo
            .store()
            .backend()
            .downcast_ref::<jj_lib::git_backend::GitBackend>()
            .ok_or("only Git-backed JJ repositories are supported")?;
        Ok(backend.git_repo().common_dir().to_path_buf())
    })
}

fn install_excludes(repo: &ReadonlyRepo) -> Result<()> {
    let backend = repo
        .store()
        .backend()
        .downcast_ref::<jj_lib::git_backend::GitBackend>()
        .ok_or("only Git-backed JJ repositories are supported")?;
    let excludes = backend.git_repo_path().join("info/exclude");
    if let Some(parent) = excludes.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::occupancy_vcs::append_exclude_lines(&excludes).map_err(|e| e.to_string())?;
    Ok(())
}

pub(super) fn mint(
    request: &super::occupancy_vcs::MintRequest<'_>,
) -> std::result::Result<MintedCopy, VcsError> {
    let super::occupancy_vcs::MintRequest {
        launch,
        agent_id,
        parent_cwd,
        base,
        namespace,
        ..
    } = *request;
    let tree8 = namespace
        .get(..8)
        .ok_or_else(|| VcsError::mint_unavailable("invalid tree namespace"))?;
    let names = super::occupancy_vcs::copy_names(tree8, agent_id);
    if super::occupancy_vcs::branch_of_copy_dir(&names.dir).is_none() {
        return Err(VcsError::mint_unavailable("invalid copy identity"));
    }
    let id = names.dir.as_str();
    let dest = launch.join(".plak/workspaces").join(id);
    let mut parent = load(parent_cwd).map_err(|error| failure(&error))?;
    let common = git_common_dir(&parent).map_err(|error| failure(&error))?;
    let namespace_guard = super::occupancy_vcs::reserve_copy_namespace(&common, namespace)?;
    let created = std::cell::Cell::new(false);
    pollster::block_on(async {
        let (repo, head) = snapshot_workspace(&mut parent).await?;
        install_excludes(&repo)?;
        let supplied_base = base.is_some();
        let base = match base {
            Some(base) => {
                repo.store()
                    .get_commit_async(
                        &CommitId::try_from_hex(&base.commit).ok_or("invalid commit identity")?,
                    )
                    .await?
            }
            None => head,
        };
        let dest = launch.join(".plak/workspaces").join(id);
        if dest.exists() {
            return Err("workspace destination already exists".into());
        }
        std::fs::create_dir_all(&dest)?;
        created.set(true);
        let name = names.jj_workspace;
        let expected_init_parent=repo.op_id().clone();
        let (mut ws, repo) = Workspace::init_workspace_with_existing_repo(
            &dest,
            parent.repo_path(),
            &repo,
            &*default_working_copy_factory(),
            WorkspaceNameBuf::from(name.clone()),
        )
        .await?;
        if repo.operation().parent_ids() != [expected_init_parent] || repo.op_heads_store().get_op_heads().await? != [repo.op_id().clone()] {
            return Err(recovery("workspace registration raced another repository operation; retained workspace needs recovery"));
        }
        let old = current(&repo, &ws).await?;
        let mut tx = repo.start_transaction();
        let base = if supplied_base {
            base
        } else {
            tx.repo_mut().new_commit(base.parent_ids().to_vec(), base.tree())
                .set_description(format!("plak base {id}"))
                .write().await?
        };
        let child = tx
            .repo_mut()
            .new_commit(vec![base.id().clone()], base.tree())
            .write()
            .await?;
        tx.repo_mut()
            .set_wc_commit(ws.workspace_name().to_owned(), child.id().clone())?;
        let repo = publish_checked(tx, "plak mint workspace").await?;
        let checkout=ws.check_out(repo.op_id().clone(), Some(&old.tree()), &child).await?;
        if checkout.skipped_files != 0 { return Err(recovery("workspace checkout skipped files; retained workspace needs recovery")); }
        let base_ref = super::occupancy_vcs::copy_base_ref(namespace, agent_id);
        pin_transaction(launch, namespace, &[(base_ref, Some(base.id().hex().as_str()))], false, true, Some(&namespace_guard))
            .map_err(|error| recovery(error.detail))?;
        drop(namespace_guard);
        crate::git_redirect_fence::install_jj(&dest, &base.id().hex())?;
        crate::git_redirect_fence::install(&dest)?;
        Ok::<_, Error>(MintedCopy {
            path: dest,
            branch: Some(name),
            base: description(&base),
        })
    })
    .map_err(|error| {
        if created.get() {
            failure(&recovery(format!(
                "workspace initialization incomplete; recovery required; retained at {}: {error}",
                dest.display()
            )))
        } else {
            failure(&error)
        }
    })
}

pub(super) fn adopt(
    target: &Path,
    child: &Path,
    base: &str,
    candidate: &VcsSnapshot,
    expected: Option<&str>,
) -> std::result::Result<AdoptLanding, AdoptFailure> {
    pollster::block_on(async {
        let mut ws = load(target).map_err(|e| AdoptFailure::Unavailable(e.to_string()))?;
        let child_ws = load(child).map_err(|e| AdoptFailure::Unavailable(e.to_string()))?;
        if ws.repo_path() != child_ws.repo_path() {
            return Err(AdoptFailure::Unavailable(
                "candidate belongs to a different JJ repository".into(),
            ));
        }
        let result: Result<_> = async {
            let (repo, head) = snapshot_workspace(&mut ws).await?;
            let before = description(&head);
            if expected.is_some_and(|e| !before.matches_expectation(e)) {
                return Ok(Err(AdoptFailure::TargetChanged(before.token())));
            }
            let base = repo
                .store()
                .get_commit_async(&CommitId::try_from_hex(base).ok_or("invalid commit identity")?)
                .await?;
            let candidate = repo
                .store()
                .get_commit_async(
                    &CommitId::try_from_hex(&candidate.commit).ok_or("invalid commit identity")?,
                )
                .await?;
            let merged = MergedTree::merge(Merge::from_removes_adds(
                vec![(base.tree(), "base".into())],
                vec![
                    (head.tree(), "target".into()),
                    (candidate.tree(), "candidate".into()),
                ],
            ))
            .await?;
            if merged.has_conflict() {
                let paths = merged
                    .conflicts()
                    .map(|(path, _)| ConflictPath {
                        path: path.as_internal_file_string().to_owned(),
                        kind: "contents".to_owned(),
                    })
                    .collect();
                return Ok(Err(AdoptFailure::Conflict { paths, candidate: None }));
            }
            let mut changes = head.tree().diff_stream(&merged, &EverythingMatcher);
            let mut files = 0u32;
            while let Some(entry) = changes.next().await {
                entry.values?;
                files = files.saturating_add(1);
            }
            let name = ws.workspace_name().to_owned();
            let mut locked = ws.start_working_copy_mutation().await?;
            if locked.locked_wc().old_tree().tree_ids_and_labels()
                != head.tree().tree_ids_and_labels()
            {
                return Err(recovery(
                    "working copy changed before adoption; recovery required",
                ));
            }
            let (checked_tree, _) = locked.locked_wc().snapshot(&snapshot_options()?).await?;
            if checked_tree.tree_ids_and_labels() != head.tree().tree_ids_and_labels() {
                return Err(recovery("working files changed after target snapshot; files preserved; recovery required"));
            }
            let latest = repo.reload_at_head().await?;
            if latest.op_id() != repo.op_id() {
                return Err(recovery(
                    "repository changed before adoption; recovery required",
                ));
            }
            let mut tx = repo.start_transaction();
            let updated = tx
                .repo_mut()
                .rewrite_commit(&head)
                .set_tree(merged)
                .write()
                .await?;
            tx.repo_mut().set_wc_commit(name, updated.id().clone())?;
            tx.repo_mut().reparent_descendants().await?;
            let repo = publish_checked(tx, "plak adopt candidate").await.map_err(|_| recovery("adoption publication uncertain; recovery required"))?;
            let stats = locked.locked_wc().check_out(&updated).await.map_err(|_| {
                recovery("adoption published but checkout failed; recovery required")
            })?;
            if stats.skipped_files != 0 {
                return Err(recovery("adoption published but checkout skipped files; recovery required"));
            }
            locked.finish(repo.op_id().clone()).await.map_err(|_| recovery("adoption published but working-copy state could not be saved; recovery required"))?;
            Ok(Ok(AdoptLanding {
                target_before: before.token(),
                target_after: description(&updated).token(),
                files_changed: files,
                files: Vec::new(),
                child_base: None,
            }))
        }
        .await;
        result.map_err(|error| adopt_failure(&error))?
    })
}

fn adopt_failure(error: &Error) -> AdoptFailure {
    if error.is::<RecoveryRequired>() {
        AdoptFailure::RecoveryRequired(error.to_string())
    } else {
        AdoptFailure::Unavailable(error.to_string())
    }
}

pub(super) fn diverged(path: &Path, base: &str) -> std::result::Result<bool, VcsError> {
    pollster::block_on(async {
        let mut ws = load(path)?;
        let (repo, head) = snapshot_workspace(&mut ws).await?;
        let base = repo
            .store()
            .get_commit_async(&CommitId::try_from_hex(base).ok_or("invalid commit identity")?)
            .await?;
        Ok::<_, Error>(head.tree().tree_ids() != base.tree().tree_ids())
    })
    .map_err(|error| failure(&error))
}

pub(super) const BUILT_LIBRARY_VERSION: Option<&str> = Some(super::JJ_LIB_VERSION);

pub(super) fn validate_copy_base(path: &Path, base: &str) -> std::result::Result<(), VcsError> {
    pollster::block_on(async {
        let ws = load(path)?;
        let repo = ws.repo_loader().load_at_head().await?;
        let id = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or("invalid copy path")?;
        let base = CommitId::try_from_hex(base).ok_or("invalid base")?;
        if pinned_base(&repo, id).as_ref() != Some(&base) {
            return Err(recovery(
                "jj copy base provenance is missing or differs; inspect the retained copy",
            ));
        }
        Ok::<_, Error>(())
    })
    .map_err(|error| failure(&error))
}

pub(super) fn history_left_base(
    path: &Path,
    base: &str,
    sealed: &str,
) -> std::result::Result<bool, VcsError> {
    pollster::block_on(async {
        let ws = load(path)?;
        let repo = ws.repo_loader().load_at_head().await?;
        let base = CommitId::try_from_hex(base).ok_or("invalid base")?;
        let sealed = CommitId::try_from_hex(sealed).ok_or("invalid sealed commit")?;
        Ok::<_, Error>(!repo.index().is_ancestor(&base, &sealed).await?)
    })
    .map_err(|error| failure(&error))
}

fn pinned_base(repo: &ReadonlyRepo, id: &str) -> Option<CommitId> {
    let backend = repo
        .store()
        .backend()
        .downcast_ref::<jj_lib::git_backend::GitBackend>()?;
    let git = backend.git_repo();
    let (namespace, agent) =
        super::occupancy_vcs::copy_namespace_owner(git.common_dir(), id).ok()?;
    pinned_base_reference(
        repo,
        &super::occupancy_vcs::copy_base_ref(&namespace, &agent),
    )
}

fn pinned_base_reference(repo: &ReadonlyRepo, reference: &str) -> Option<CommitId> {
    let backend = repo
        .store()
        .backend()
        .downcast_ref::<jj_lib::git_backend::GitBackend>()?;
    let git = backend.git_repo();
    let reference = git.try_find_reference(reference).ok()??;
    CommitId::try_from_hex(reference.target().try_id()?.to_string())
}

pub(super) fn forget(
    launch: &Path,
    id: &str,
    namespace: &str,
) -> std::result::Result<(), VcsError> {
    let expected_dir = super::occupancy_vcs::branch_of_copy_dir(id)
        .ok_or_else(|| VcsError::mint_unavailable("unrecognized copy identity; retained"))?;
    let (tree8, agent) = expected_dir
        .strip_prefix("plak/")
        .and_then(|tail| tail.split_once('/'))
        .ok_or_else(|| VcsError::mint_unavailable("invalid copy branch; retained"))?;
    if namespace.get(..8) != Some(tree8) {
        return Err(VcsError::mint_unavailable(
            "copy belongs to another namespace; retained",
        ));
    }
    let names = super::occupancy_vcs::copy_names(tree8, agent);
    pollster::block_on(async {
        let ws = load(launch)?;
        let repo = ws.repo_loader().load_at_head().await?;
        let backend = repo
            .store()
            .backend()
            .downcast_ref::<jj_lib::git_backend::GitBackend>()
            .ok_or("only Git-backed JJ repositories are supported")?;
        let namespace_guard = super::occupancy_vcs::reserve_copy_namespace(
            backend.git_repo().common_dir(),
            namespace,
        )
        .map_err(|error| recovery(error.detail))?;
        let base_ref = super::occupancy_vcs::copy_base_ref(namespace, agent);
        let base = pinned_base_reference(&repo, &base_ref);
        let mut tx = repo.start_transaction();
        tx.repo_mut()
            .remove_workspace(&WorkspaceNameBuf::from(names.jj_workspace))
            .await?;
        tx.repo_mut().rebase_descendants().await?;
        if let Some(base) = base {
            let expression = jj_lib::revset::ResolvedRevsetExpression::commits(vec![base.clone()]);
            let descendants = expression
                .descendants()
                .minus(&expression)
                .evaluate(tx.repo())?;
            if descendants.stream().next().await.is_none() {
                let base = repo.store().get_commit_async(&base).await?;
                if base.description().starts_with("plak base ") {
                    tx.repo_mut().record_abandoned_commit(&base);
                    tx.repo_mut().rebase_descendants().await?;
                }
            }
        }
        publish_checked(tx, "plak forget workspace").await?;
        pin_transaction(
            launch,
            namespace,
            &[(base_ref, None)],
            false,
            true,
            Some(&namespace_guard),
        )
        .map_err(|error| recovery(error.detail))?;
        Ok::<_, Error>(())
    })
    .map_err(|error| failure(&error))
}

#[cfg(test)]
pub(super) fn resolve(path: &Path, commit: &str) -> Option<String> {
    pollster::block_on(async {
        let ws = load(path).ok()?;
        let repo = ws.repo_loader().load_at_head().await.ok()?;
        let commit = repo
            .store()
            .get_commit_async(&CommitId::try_from_hex(commit)?)
            .await
            .ok()?;
        Some(commit.id().hex())
    })
}

pub(super) fn over(path: &Path, id: &str, candidate: &VcsSnapshot) -> bool {
    pollster::block_on(async {
        let ws = load(path)?;
        let repo = ws.repo_loader().load_at_head().await?;
        let backend = repo
            .store()
            .backend()
            .downcast_ref::<jj_lib::git_backend::GitBackend>()
            .ok_or("only Git-backed JJ repositories are supported")?;
        let (namespace, agent) =
            super::occupancy_vcs::copy_namespace_owner(backend.git_repo().common_dir(), id)
                .map_err(|error| error.detail)?;
        let name = WorkspaceNameBuf::from(
            super::occupancy_vcs::copy_names(&namespace[..8], &agent).jj_workspace,
        );
        let child = repo.view().get_wc_commit_id(&name).ok_or("copy missing")?;
        let child = repo.store().get_commit_async(child).await?;
        let ancestor =
            CommitId::try_from_hex(&candidate.commit).ok_or("invalid candidate identity")?;
        Ok::<_, Error>(repo.index().is_ancestor(&ancestor, child.id()).await?)
    })
    .unwrap_or(false)
}

pub(super) fn dirty(path: &Path) -> bool {
    pollster::block_on(async {
        let mut ws = load(path)?;
        let (repo, head) = snapshot_workspace(&mut ws).await?;
        let parent = head.parent_ids().first().ok_or("parent missing")?;
        let parent = repo.store().get_commit_async(parent).await?;
        Ok::<_, Error>(head.tree().tree_ids() != parent.tree().tree_ids())
    })
    .unwrap_or(true)
}

async fn revset_count(
    repo: &ReadonlyRepo,
    left: &CommitId,
    right: &[CommitId],
    excluded_tree: Option<&MergedTree>,
) -> Result<u64> {
    let left = jj_lib::revset::ResolvedRevsetExpression::commits(vec![left.clone()]).ancestors();
    let right = jj_lib::revset::ResolvedRevsetExpression::commits(right.to_vec()).ancestors();
    let selected = left.minus(&right).evaluate(repo)?;
    let mut stream = selected.stream();
    let mut count = 0u64;
    while let Some(id) = stream.next().await {
        let id = id?;
        let commit = repo.store().get_commit_async(&id).await?;
        if !commit.is_empty(repo).await?
            && excluded_tree.is_none_or(|tree| {
                tree.tree_ids_and_labels() != commit.tree().tree_ids_and_labels()
            })
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}
pub(super) fn ahead_behind(child: &Path, parent: &Path, base: Option<&str>) -> Option<(u64, u64)> {
    pollster::block_on(async {
        let base = CommitId::try_from_hex(base?)?;
        let child = load(child).ok()?;
        let parent = load(parent).ok()?;
        if child.repo_path() != parent.repo_path() {
            return None;
        }
        let repo = parent.repo_loader().load_at_head().await.ok()?;
        let child = current(&repo, &child).await.ok()?;
        let parent = current(&repo, &parent).await.ok()?;
        Some((
            revset_count(&repo, child.id(), std::slice::from_ref(&base), None)
                .await
                .ok()?,
            revset_count(
                &repo,
                parent.id(),
                repo.store()
                    .get_commit_async(&base)
                    .await
                    .ok()?
                    .parent_ids(),
                Some(&repo.store().get_commit_async(&base).await.ok()?.tree()),
            )
            .await
            .ok()?,
        ))
    })
}

#[cfg(test)]
pub(super) fn pin(
    path: &Path,
    namespace: &str,
    id: &str,
    commit: Option<&str>,
) -> std::result::Result<(), VcsError> {
    pin_transaction(
        path,
        namespace,
        &[(super::occupancy_vcs::candidate_ref(namespace, id), commit)],
        id.starts_with("failed-"),
        false,
        None,
    )
}

pub(super) fn pin_references(
    path: &Path,
    namespace: &str,
    refs: &[(String, Option<&str>)],
    create_only: bool,
) -> std::result::Result<(), VcsError> {
    pin_transaction(path, namespace, refs, create_only, true, None)
}

fn pin_transaction(
    path: &Path,
    namespace: &str,
    refs: &[(String, Option<&str>)],
    create_only: bool,
    overwrite: bool,
    reservation: Option<&std::fs::File>,
) -> std::result::Result<(), VcsError> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    pollster::block_on(async {
        let ws = load(path)?;
        let repo = ws.repo_loader().load_at_head().await?;
        let backend = repo
            .store()
            .backend()
            .downcast_ref::<jj_lib::git_backend::GitBackend>()
            .ok_or("only Git-backed JJ repositories are supported")?;
        let git = backend.git_repo();
        let _namespace_guard = if reservation.is_none() {
            Some(super::occupancy_vcs::reserve_candidate_namespace(git.common_dir(), namespace)
                .map_err(|error| recovery(&error.detail))?)
        } else { None };
        let mut edits = Vec::new();
        for (reference, commit) in refs {
            let change = if let Some(commit) = commit {
                let target = gix::refs::Target::Object(gix::ObjectId::from_hex(commit.as_bytes())?);
                Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: "plak candidate pin".into(),
                    },
                    expected: if create_only {
                        // MustNotExist accepts an identical existing target in gix;
                        // matching the impossible null target is strictly create-only.
                        PreviousValue::ExistingMustMatch(gix::refs::Target::Object(
                            gix::ObjectId::null(git.object_hash()),
                        ))
                    } else if overwrite {
                        PreviousValue::Any
                    } else {
                        PreviousValue::ExistingMustMatch(target.clone())
                    },
                    new: target,
                }
            } else if let Some(existing) = git.try_find_reference(reference.as_str())? {
                Change::Delete {
                    expected: PreviousValue::MustExistAndMatch(existing.target().into()),
                    log: RefLog::AndReference,
                }
            } else {
                continue;
            };
            edits.push(RefEdit {
                name: reference.as_str().try_into()?,
                deref: false,
                change,
            });
        }
        let signature = gix::actor::SignatureRef {
            name: gix::bstr::BStr::new("plak"),
            email: gix::bstr::BStr::new("plak@local"),
            time: "0 +0000",
        };
        git.edit_references_as(edits, Some(signature))
            .map_err(|_| {
                let names = refs.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ");
                recovery(format!("provenance pins {names} update refused or publication uncertain; recovery required"))
            })?;
        Ok::<_, Error>(())
    })
    .map_err(|error| failure(&error))
}

#[cfg(test)]
mod tests;
