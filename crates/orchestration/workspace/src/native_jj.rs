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

use super::occupancy_vcs::{AdoptFailure, AdoptLanding, MintedCopy, VcsError, VcsSnapshot};

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

pub(super) fn mint(
    launch: &Path,
    id: &str,
    parent: &Path,
    base: Option<&VcsSnapshot>,
) -> std::result::Result<MintedCopy, VcsError> {
    let dest = launch.join(".plak/workspaces").join(id);
    let created = std::cell::Cell::new(false);
    pollster::block_on(async {
        let mut parent = load(parent)?;
        let (repo, head) = snapshot_workspace(&mut parent).await?;
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
        let name = format!("plak-{id}");
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
        pin_base(launch, id, Some(&base.id().hex())).map_err(|error| recovery(error.detail))?;
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
                    .map(|(path, _)| path.as_internal_file_string().to_owned())
                    .collect();
                return Ok(Err(AdoptFailure::Conflict(paths)));
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
                child_base: None,
            }))
        }
        .await;
        result.map_err(|e| {
            if e.is::<RecoveryRequired>() {
                AdoptFailure::RecoveryRequired(e.to_string())
            } else {
                AdoptFailure::Unavailable(e.to_string())
            }
        })?
    })
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
                "jj copy minted by an older build; discard or close it and respawn",
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

fn base_reference(id: &str) -> String {
    let prefix = id.split_once('-').map_or(id, |(namespace, _)| namespace);
    let namespace = if prefix.len() == 64 && prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        prefix.to_owned()
    } else {
        super::occupancy_vcs::digest_text(id)
    };
    format!("refs/plak/{}/bases/{id}", &namespace[..16])
}

fn pinned_base(repo: &ReadonlyRepo, id: &str) -> Option<CommitId> {
    let backend = repo
        .store()
        .backend()
        .downcast_ref::<jj_lib::git_backend::GitBackend>()?;
    let git = backend.git_repo();
    let reference = git.try_find_reference(base_reference(id).as_str()).ok()??;
    CommitId::try_from_hex(reference.target().try_id()?.to_string())
}

pub(super) fn forget(launch: &Path, id: &str) -> std::result::Result<(), VcsError> {
    pollster::block_on(async {
        let ws = load(launch)?;
        let repo = ws.repo_loader().load_at_head().await?;
        let base = pinned_base(&repo, id);
        let mut tx = repo.start_transaction();
        tx.repo_mut()
            .remove_workspace(&WorkspaceNameBuf::from(format!("plak-{id}")))
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
        pin_base(launch, id, None).map_err(|error| recovery(error.detail))?;
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
        let name = WorkspaceNameBuf::from(format!("plak-{id}"));
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

pub(super) fn pin(
    path: &Path,
    namespace: &str,
    id: &str,
    commit: Option<&str>,
) -> std::result::Result<(), VcsError> {
    pin_reference(
        path,
        namespace,
        &super::occupancy_vcs::candidate_ref(namespace, id),
        id.starts_with("failed-"),
        false,
        commit,
    )
}

fn pin_base(path: &Path, id: &str, commit: Option<&str>) -> std::result::Result<(), VcsError> {
    let reference = base_reference(id);
    let prefix = id.split_once('-').map_or(id, |(namespace, _)| namespace);
    let namespace = if prefix.len() == 64 && prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        prefix.to_owned()
    } else {
        super::occupancy_vcs::digest_text(id)
    };
    pin_reference(path, &namespace, &reference, false, true, commit)
}

fn pin_reference(
    path: &Path,
    namespace: &str,
    reference: &str,
    create_only: bool,
    overwrite: bool,
    commit: Option<&str>,
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
        let _namespace_guard = if commit.is_some() {
            Some(
                super::occupancy_vcs::reserve_candidate_namespace(git.common_dir(), namespace)
                    .map_err(|error| recovery(&error.detail))?,
            )
        } else {
            None
        };
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
        } else if let Some(existing) = git.try_find_reference(reference)? {
            Change::Delete {
                expected: PreviousValue::MustExistAndMatch(existing.target().into()),
                log: RefLog::AndReference,
            }
        } else {
            return Ok(());
        };
        let signature = gix::actor::SignatureRef {
            name: gix::bstr::BStr::new("plak"),
            email: gix::bstr::BStr::new("plak@local"),
            time: "0 +0000",
        };
        git.edit_references_as(
            [RefEdit {
                name: reference.try_into()?,
                deref: false,
                change,
            }],
            Some(signature),
        )
        .map_err(|_| {
            recovery(format!(
                "candidate pin {reference} update refused or publication uncertain; recovery required"
            ))
        })?;
        Ok::<_, Error>(())
    })
    .map_err(|error| failure(&error))
}

#[cfg(test)]
mod tests {
    use super::{adopt, mint, snapshot};
    use crate::vcs_fixtures::{jj_available, jj_in, jj_repo};
    use jj_lib::object_id::ObjectId;
    use jj_lib::repo::Repo;
    #[test]
    fn jj_counts_exclude_empty_commits_and_the_copied_mint_tree() {
        if !jj_available() {
            return;
        }
        let root = jj_repo();
        let clean = mint(root.path(), "clean-count", root.path(), None).unwrap();
        assert_eq!(
            super::ahead_behind(&clean.path, root.path(), Some(&clean.base.commit)),
            Some((0, 0))
        );
        std::fs::write(root.path().join("dirty.txt"), "at mint").unwrap();
        let child = mint(root.path(), "dirty-count", root.path(), None).unwrap();
        assert_eq!(
            super::ahead_behind(&child.path, root.path(), Some(&child.base.commit)),
            Some((0, 0))
        );
        std::fs::write(root.path().join("dirty.txt"), "after mint").unwrap();
        snapshot(root.path()).unwrap();
        assert_eq!(
            super::ahead_behind(&child.path, root.path(), Some(&child.base.commit)),
            Some((0, 1))
        );
        for n in 1..=3 {
            std::fs::write(child.path.join(format!("child-{n}.txt")), "child").unwrap();
            jj_in(&child.path, &["commit", "-m", "child work"]);
            assert_eq!(
                super::ahead_behind(&child.path, root.path(), Some(&child.base.commit)),
                Some((n, 1))
            );
        }
    }

    #[test]
    fn upgraded_jj_copy_requires_a_matching_base_pin_and_a_recorded_row() {
        let root = native_repo();
        let child = mint(root.path(), "base-validation", root.path(), None).unwrap();
        assert!(super::validate_copy_base(&child.path, &child.base.commit).is_ok());
        let head = snapshot(&child.path).unwrap();
        assert!(
            super::validate_copy_base(&child.path, &head.commit)
                .unwrap_err()
                .detail
                .contains("jj copy minted by an older build")
        );
        let missing = crate::git_redirect_fence::install_for_copy(&child.path, None).unwrap_err();
        assert!(missing.contains("no recorded tree row"));
        assert!(
            super::validate_copy_base(&child.path, &snapshot(root.path()).unwrap().commit).is_err()
        );
        super::pin_base(&child.path, "base-validation", None).unwrap();
        assert!(
            super::validate_copy_base(&child.path, &child.base.commit)
                .unwrap_err()
                .detail
                .contains("jj copy minted by an older build")
        );
    }

    #[test]
    fn stale_base_pin_is_replaced_and_unused_frozen_base_is_abandoned() {
        let root = native_repo();
        let stale = snapshot(root.path()).unwrap();
        super::pin_base(root.path(), "reused-pin", Some(&stale.commit)).unwrap();
        let child = mint(root.path(), "reused-pin", root.path(), None).unwrap();
        assert_ne!(child.base.commit, stale.commit);
        super::forget(root.path(), "reused-pin").unwrap();
        pollster::block_on(async {
            let ws = super::load(root.path()).unwrap();
            let repo = ws.repo_loader().load_at_head().await.unwrap();
            assert!(super::pinned_base(&repo, "reused-pin").is_none());
            assert!(
                !repo.view().heads().contains(
                    &jj_lib::backend::CommitId::try_from_hex(&child.base.commit).unwrap()
                )
            );
        });
    }

    #[test]
    fn git_discovery_ceiling_is_honored_by_environment_aware_gix() {
        if std::env::var_os("PLAK_TEST_GIX_CEILING_PROBE").is_some() {
            assert!(
                gix::ThreadSafeRepository::discover_with_environment_overrides_opts(
                    ".",
                    gix::discover::upwards::Options::default(),
                    gix::sec::trust::Mapping::default(),
                )
                .is_err()
            );
            return;
        }
        let launch = crate::vcs_fixtures::git_repo();
        let copy = launch.path().join("copies/child");
        std::fs::create_dir_all(copy.join("sub")).unwrap();
        assert!(gix::discover(&copy).is_ok());
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "native_jj::tests::git_discovery_ceiling_is_honored_by_environment_aware_gix"
                ])
                .current_dir(copy.join("sub"))
                .env_clear()
                .env("GIT_CEILING_DIRECTORIES", copy.parent().unwrap())
                .env("PLAK_TEST_GIX_CEILING_PROBE", "1")
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn running_forks_survive_parent_edit_and_snapshot() {
        let repo = native_repo();
        let first = mint(repo.path(), "first", repo.path(), None).unwrap();
        let second = mint(repo.path(), "second", repo.path(), None).unwrap();
        std::fs::write(first.path.join("SEED.txt"), "first work").unwrap();
        std::fs::write(second.path.join("new.txt"), "second work").unwrap();
        std::fs::write(repo.path().join("SEED.txt"), "parent work").unwrap();
        snapshot(repo.path()).unwrap();
        for child in [&first, &second] {
            assert_ne!(snapshot(&child.path).unwrap().content, child.base.content);
            assert_eq!(
                super::resolve(repo.path(), &child.base.commit).as_deref(),
                Some(child.base.commit.as_str())
            );
        }
        assert_eq!(
            std::fs::read_to_string(first.path.join("SEED.txt")).unwrap(),
            "first work"
        );
        assert_eq!(
            std::fs::read_to_string(second.path.join("new.txt")).unwrap(),
            "second work"
        );
    }

    #[test]
    fn owner_describe_commit_and_new_do_not_stale_forks() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let child = mint(repo.path(), "owner-commands", repo.path(), None).unwrap();
        std::fs::write(child.path.join("new.txt"), "child work").unwrap();
        for args in [
            vec!["describe", "-m", "owner"],
            vec!["commit", "-m", "owner"],
            vec!["new"],
        ] {
            jj_in(repo.path(), &args);
            snapshot(&child.path).unwrap();
            assert_eq!(
                std::fs::read_to_string(child.path.join("new.txt")).unwrap(),
                "child work"
            );
        }
    }

    #[test]
    fn mint_base_is_a_sibling_of_parent_working_copy() {
        let root = native_repo();
        let child = mint(root.path(), "sibling-base", root.path(), None).unwrap();
        pollster::block_on(async {
            let ws = super::load(root.path()).unwrap();
            let repo = ws.repo_loader().load_at_head().await.unwrap();
            let parent = super::current(&repo, &ws).await.unwrap();
            let base = repo
                .store()
                .get_commit_async(
                    &jj_lib::backend::CommitId::try_from_hex(&child.base.commit).unwrap(),
                )
                .await
                .unwrap();
            let child_ws = super::load(&child.path).unwrap();
            let head = super::current(&repo, &child_ws).await.unwrap();
            assert_eq!(base.parent_ids(), parent.parent_ids());
            assert_ne!(base.id(), parent.id());
            assert_eq!(base.tree().tree_ids(), parent.tree().tree_ids());
            assert_eq!(head.parent_ids(), &[base.id().clone()]);
            let git = repo
                .store()
                .backend()
                .downcast_ref::<jj_lib::git_backend::GitBackend>()
                .unwrap()
                .git_repo();
            let namespace = crate::occupancy_vcs::digest_text("sibling-base");
            let reference = format!("refs/plak/{}/bases/sibling-base", &namespace[..16]);
            assert!(git.try_find_reference(&reference).unwrap().is_some());
        });
        super::forget(root.path(), "sibling-base").unwrap();
        pollster::block_on(async {
            let ws = super::load(root.path()).unwrap();
            let repo = ws.repo_loader().load_at_head().await.unwrap();
            let git = repo
                .store()
                .backend()
                .downcast_ref::<jj_lib::git_backend::GitBackend>()
                .unwrap()
                .git_repo();
            let namespace = crate::occupancy_vcs::digest_text("sibling-base");
            assert!(
                git.try_find_reference(
                    format!("refs/plak/{}/bases/sibling-base", &namespace[..16]).as_str()
                )
                .unwrap()
                .is_none()
            );
        });
    }
    #[test]
    fn native_adoption_preserves_child_and_supports_followup() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let child = mint(repo.path(), "native", repo.path(), None).unwrap();
        std::fs::write(child.path.join("first.txt"), "first\n").unwrap();
        let first = snapshot(&child.path).unwrap();
        let landing = adopt(repo.path(), &child.path, &child.base.commit, &first, None).unwrap();
        assert!(landing.child_base.is_none());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("first.txt")).unwrap(),
            "first\n"
        );
        assert_eq!(snapshot(&child.path).unwrap().content, first.content);
        std::fs::write(child.path.join("second.txt"), "second\n").unwrap();
        let second = snapshot(&child.path).unwrap();
        adopt(repo.path(), &child.path, &first.commit, &second, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("second.txt")).unwrap(),
            "second\n"
        );
        jj_in(repo.path(), &["status"]);
        jj_in(&child.path, &["status"]);
    }
    #[test]
    fn parent_rewrite_refuses_recovery_and_preserves_dirty_and_untracked_files() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let child = mint(repo.path(), "preserve", repo.path(), None).unwrap();
        std::fs::write(child.path.join("SEED.txt"), "private child edit\n").unwrap();
        std::fs::write(child.path.join("untracked.txt"), "untracked child work\n").unwrap();
        jj_in(repo.path(), &["abandon", "-r", &child.base.commit]);
        let result = snapshot(&child.path);
        assert_eq!(
            std::fs::read_to_string(child.path.join("SEED.txt")).unwrap(),
            "private child edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(child.path.join("untracked.txt")).unwrap(),
            "untracked child work\n"
        );
        match result {
            Err(error) => assert_eq!(
                error.kind,
                crate::occupancy_vcs::VcsErrorKind::RecoveryRequired
            ),
            Ok(saved) => assert_eq!(snapshot(&child.path).unwrap().content, saved.content),
        }
    }
    fn native_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        pollster::block_on(jj_lib::workspace::Workspace::init_internal_git(
            &super::settings().unwrap(),
            repo.path(),
            "sha1".parse().unwrap(),
        ))
        .unwrap();
        std::fs::write(repo.path().join("SEED.txt"), "seed\n").unwrap();
        snapshot(repo.path()).unwrap();
        repo
    }
    #[test]
    fn native_repository_snapshot_mint_and_adopt_need_no_cli_commands() {
        let repo = native_repo();
        let child = mint(repo.path(), "native-only", repo.path(), None).unwrap();
        std::fs::write(child.path.join("result.txt"), "done").unwrap();
        let result = snapshot(&child.path).unwrap();
        adopt(repo.path(), &child.path, &child.base.commit, &result, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("result.txt")).unwrap(),
            "done"
        );
    }
    #[test]
    fn ancestor_rewrite_still_refuses_and_preserves_files() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let child = mint(repo.path(), "stale", repo.path(), None).unwrap();
        std::fs::write(child.path.join("SEED.txt"), "child dirty").unwrap();
        std::fs::write(child.path.join("untracked.txt"), "child untracked").unwrap();
        jj_in(repo.path(), &["edit", &child.base.commit]);
        std::fs::write(repo.path().join("SEED.txt"), "ancestor tree rewrite").unwrap();
        jj_in(repo.path(), &["status"]);
        for _ in 0..2 {
            let error = snapshot(&child.path).unwrap_err();
            assert_eq!(
                error.kind,
                crate::occupancy_vcs::VcsErrorKind::RecoveryRequired
            );
            assert_eq!(
                std::fs::read_to_string(child.path.join("SEED.txt")).unwrap(),
                "child dirty"
            );
            assert_eq!(
                std::fs::read_to_string(child.path.join("untracked.txt")).unwrap(),
                "child untracked"
            );
        }
    }
    #[test]
    fn external_git_commit_import_and_native_adopt_preserve_staging() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@local",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        std::fs::write(repo.path().join("external.txt"), "external commit").unwrap();
        git(&["add", "external.txt"]);
        git(&["commit", "-m", "external"]);
        let git_head = git(&["rev-parse", "HEAD"]);
        std::fs::write(repo.path().join("staged.txt"), "staged").unwrap();
        git(&["add", "staged.txt"]);
        std::fs::write(repo.path().join("staged.txt"), "unstaged remainder").unwrap();
        let index = std::fs::read(repo.path().join(".git/index")).unwrap();
        let imported = snapshot(repo.path()).unwrap();
        pollster::block_on(async {
            let ws = super::load(repo.path()).unwrap();
            let view = ws.repo_loader().load_at_head().await.unwrap();
            let head = super::current(&view, &ws).await.unwrap();
            assert_eq!(
                head.parent_ids()[0].hex(),
                String::from_utf8(git_head.clone()).unwrap().trim()
            );
        });
        assert_eq!(
            std::fs::read(repo.path().join(".git/index")).unwrap(),
            index
        );
        let child = mint(repo.path(), "gitinterop", repo.path(), Some(&imported)).unwrap();
        std::fs::write(child.path.join("result.txt"), "child result").unwrap();
        let result = snapshot(&child.path).unwrap();
        adopt(repo.path(), &child.path, &child.base.commit, &result, None).unwrap();
        assert_eq!(git(&["rev-parse", "HEAD"]), git_head);
        assert_eq!(
            std::fs::read(repo.path().join(".git/index")).unwrap(),
            index
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("staged.txt")).unwrap(),
            "unstaged remainder"
        );
    }
    #[test]
    fn publication_refuses_an_external_operation_after_evaluation() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        pollster::block_on(async {
            let ws = super::load(repo.path()).unwrap();
            let view = ws.repo_loader().load_at_head().await.unwrap();
            let tx = view.start_transaction();
            jj_in(repo.path(), &["describe", "-m", "external operation wins"]);
            let error = super::publish_checked(tx, "must not overwrite external operation")
                .await
                .unwrap_err();
            assert!(error.is::<super::RecoveryRequired>());
            let after = ws.repo_loader().load_at_head().await.unwrap();
            assert_eq!(
                super::current(&after, &ws).await.unwrap().description(),
                "external operation wins\n"
            );
        });
    }
    #[test]
    fn secret_policy_allows_example_and_excludes_new_credentials() {
        let repo = native_repo();
        let before = snapshot(repo.path()).unwrap();
        for name in [
            ".env",
            ".ENV.production",
            "credentials.json",
            "id_ed25519",
            "private.pfx",
        ] {
            std::fs::write(repo.path().join(name), "fixture only").unwrap();
        }
        assert_eq!(snapshot(repo.path()).unwrap().content, before.content);
        std::fs::write(repo.path().join(".env.example"), "example").unwrap();
        assert_ne!(snapshot(repo.path()).unwrap().content, before.content);
    }
    #[test]
    fn native_tree_identity_frames_order_and_conflict_labels() {
        let repo = native_repo();
        pollster::block_on(async {
            let ws = super::load(repo.path()).unwrap();
            let repo = ws.repo_loader().load_at_head().await.unwrap();
            let ids = vec![
                jj_lib::backend::TreeId::from_bytes(&[1; 20]),
                jj_lib::backend::TreeId::from_bytes(&[2; 20]),
                jj_lib::backend::TreeId::from_bytes(&[3; 20]),
            ];
            let tree = |ids: Vec<jj_lib::backend::TreeId>, labels: Vec<String>| {
                jj_lib::merged_tree::MergedTree::new(
                    repo.store().clone(),
                    jj_lib::merge::Merge::from_vec(ids),
                    jj_lib::conflict_labels::ConflictLabels::from_vec(labels),
                )
            };
            let labels = vec!["left".into(), "base".into(), "right".into()];
            let first = super::tree_content(&tree(ids.clone(), labels.clone()));
            let mut reordered = ids.clone();
            reordered.swap(0, 2);
            assert_ne!(first, super::tree_content(&tree(reordered, labels)));
            assert_ne!(
                first,
                super::tree_content(&tree(
                    ids,
                    vec!["other".into(), "base".into(), "right".into()]
                ))
            );
            assert_eq!(
                first,
                "jj-tree-v1:b97ecdb4836cc06c05cd4317369828a992fdaef4564aac688ec6446642d2506a"
            );
        });
    }
    #[test]
    fn candidate_ref_stays_exact_after_rewrite_and_workspace_removal() {
        let repo = native_repo();
        let child = mint(repo.path(), "pinned", repo.path(), None).unwrap();
        std::fs::write(child.path.join("candidate.txt"), "candidate").unwrap();
        let candidate = crate::occupancy_vcs::seal_candidate(
            &child.path,
            &super::super::occupancy_vcs::digest_text("fixture"),
            "candidate-pinned",
        )
        .unwrap();
        std::fs::write(child.path.join("candidate.txt"), "later work").unwrap();
        let changed = snapshot(&child.path).unwrap();
        assert!(
            super::pin(
                &child.path,
                &super::super::occupancy_vcs::digest_text("fixture"),
                "candidate-pinned",
                Some(&changed.commit)
            )
            .is_err()
        );
        super::forget(repo.path(), "pinned").unwrap();
        pollster::block_on(async {
            let ws = super::load(repo.path()).unwrap();
            let repo = ws.repo_loader().load_at_head().await.unwrap();
            let backend = repo
                .store()
                .backend()
                .downcast_ref::<jj_lib::git_backend::GitBackend>()
                .unwrap();
            let git = backend.git_repo();
            let reference = git
                .find_reference(
                    super::super::occupancy_vcs::candidate_ref(
                        &super::super::occupancy_vcs::digest_text("fixture"),
                        "candidate-pinned",
                    )
                    .as_str(),
                )
                .unwrap();
            assert_eq!(
                reference.target().try_id().unwrap().to_string(),
                candidate.commit
            );
        });
    }
}
