//! Clean-merge live-fork catch-up, with exact-state recovery after publication.

use std::path::PathBuf;

use super::{ConflictPath, SessionVcs, VcsError, VcsErrorKind, git, namespace, pins};

/// One idle live fork and its immutable current base.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RefreshRequest {
    /// Actual parent's live checkout.
    pub parent: PathBuf,
    /// Child checkout receiving the catch-up.
    pub child: PathBuf,
    /// Recorded child merge base.
    pub base: String,
    /// Tree-owned attached branch, without `refs/heads/`.
    pub branch: String,
    /// Full tree digest owning the base pin.
    pub namespace: String,
    /// Producer whose base pin is advanced.
    pub agent: String,
}

/// Object-only preparation; only `Ready` requires a durable apply intent.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RefreshPreparation {
    /// Parent has no new tree content at this base.
    Unchanged,
    /// The copy is temporarily ineligible or its backend lacks refresh.
    Skipped(String),
    /// The whole-worktree merge conflicts; nothing changed.
    Conflict(Vec<ConflictPath>),
    /// Exact inputs and results which must be published before application.
    Ready(Box<RefreshPlan>),
}

/// Exact content and branch identities across the refresh's apply boundary.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RefreshPlan {
    /// Paths, ownership and original base.
    pub request: RefreshRequest,
    /// Parent snapshot to become the new base.
    pub parent: String,
    /// Parent tree identity for the workspace row.
    pub parent_content: String,
    /// Exact child snapshot before application.
    pub child: String,
    /// Exact child snapshot before application.
    pub child_content: String,
    /// Exact whole-worktree result.
    pub merged_content: String,
    /// Attached branch's old commit.
    pub head_old: String,
    /// Parent-tree checkpoint, parented by old HEAD and the parent snapshot.
    pub merge_commit: String,
}

/// Settled application, including a proven no-write abandonment.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RefreshApplied {
    /// Branch, index and base pin now agree with the plan.
    Refreshed,
    /// The original child content survived; the old base remains authoritative.
    Abandoned(String),
}

fn query(path: &std::path::Path, args: &[&str]) -> Result<String, VcsError> {
    Ok(String::from_utf8_lossy(&query_bytes(path, args)?)
        .trim()
        .to_owned())
}

fn query_bytes(path: &std::path::Path, args: &[&str]) -> Result<Vec<u8>, VcsError> {
    let output = git::git_query(path, args)
        .map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
    if !output.status.success() {
        return Err(VcsError::mint_unavailable(git::stderr_excerpt(
            &output.stderr,
        )));
    }
    Ok(output.stdout)
}

pub(super) fn prepare(request: &RefreshRequest) -> Result<RefreshPreparation, VcsError> {
    if SessionVcs::of_copy(&request.child)? != SessionVcs::Git {
        return Ok(RefreshPreparation::Skipped(
            "refresh is not yet available for jj copies".into(),
        ));
    }
    if query(&request.child, &["symbolic-ref", "--quiet", "HEAD"])
        .ok()
        .as_deref()
        != Some(format!("refs/heads/{}", request.branch).as_str())
    {
        return Ok(RefreshPreparation::Skipped(
            "child HEAD is not attached to its own branch".into(),
        ));
    }
    if !query_bytes(&request.child, &["ls-files", "--unmerged", "-z"])?.is_empty() {
        return Ok(RefreshPreparation::Skipped(
            "child index has unmerged entries".into(),
        ));
    }
    let staged = query_bytes(
        &request.child,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
            "--name-only",
            "-z",
        ],
    )?;
    let unstaged = query_bytes(
        &request.child,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "-z",
        ],
    )?;
    if staged
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .any(|name| unstaged.split(|byte| *byte == 0).any(|other| name == other))
    {
        return Ok(RefreshPreparation::Skipped(
            "child has independent staged content".into(),
        ));
    }
    let parent = git::git_snapshot(&request.parent, "refresh-parent")?;
    if parent.content
        == query(
            &request.child,
            &["rev-parse", &format!("{}^{{tree}}", request.base)],
        )?
    {
        return Ok(RefreshPreparation::Unchanged);
    }
    let child = git::git_snapshot(&request.child, "refresh-child")?;
    let head = query(&request.child, &["rev-parse", "HEAD"])?;
    let merged_content = match merge(request, &parent.commit, &child.commit)? {
        Ok(whole) => whole,
        Err(paths) => return Ok(RefreshPreparation::Conflict(paths)),
    };
    // The logical base may include adopted uncommitted work absent from HEAD.
    // Checkpoint P, not C: the child's remaining delta stays uncommitted.
    let merge_commit = merge_commit(&request.child, &parent.content, &head, &parent.commit)?;
    Ok(RefreshPreparation::Ready(Box::new(RefreshPlan {
        request: request.clone(),
        parent: parent.commit,
        parent_content: parent.content,
        child: child.commit,
        child_content: child.content,
        merged_content,
        head_old: head,
        merge_commit,
    })))
}

fn merge(
    request: &RefreshRequest,
    parent: &str,
    child: &str,
) -> Result<Result<String, Vec<ConflictPath>>, VcsError> {
    let output = git::git_run(
        &request.child,
        &[
            "merge-tree",
            "--write-tree",
            "-z",
            &format!("--merge-base={}", request.base),
            parent,
            child,
        ],
    )
    .map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.code() == Some(1) {
        return Ok(Err(git::parse_merge_tree_conflicts(&stdout)));
    }
    if !output.status.success() {
        return Err(VcsError::mint_unavailable(git::stderr_excerpt(
            &output.stderr,
        )));
    }
    Ok(Ok(stdout.split('\0').next().unwrap_or_default().to_owned()))
}

fn merge_commit(
    path: &std::path::Path,
    tree: &str,
    head: &str,
    parent: &str,
) -> Result<String, VcsError> {
    let mut command = git::git_command(path);
    command
        .args([
            "commit-tree",
            tree,
            "-p",
            head,
            "-p",
            parent,
            "-m",
            "plak refresh onto parent state",
        ])
        .env("GIT_AUTHOR_NAME", "plak")
        .env("GIT_AUTHOR_EMAIL", "plak@local")
        .env("GIT_COMMITTER_NAME", "plak")
        .env("GIT_COMMITTER_EMAIL", "plak@local")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_DATE", "@0 +0000");
    let output = git::output_within(command, git::GIT_WRITE_TIMEOUT)
        .map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
    if !output.status.success() {
        return Err(VcsError::mint_unavailable("cannot preserve refresh merge"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn recovery(plan: &RefreshPlan, reason: &str) -> VcsError {
    VcsError::new(
        VcsErrorKind::RecoveryRequired,
        format!(
            "refresh needs recovery ({reason}); base {}, parent {}, child {}, merged {}",
            plan.request.base, plan.parent, plan.child, plan.merged_content
        ),
    )
}

pub(super) fn apply(plan: &RefreshPlan, recover: bool) -> Result<RefreshApplied, VcsError> {
    let request = &plan.request;
    let _guard = namespace::reserve(&pins::common(&request.child)?, &request.namespace)?;
    let attached = query(&request.child, &["symbolic-ref", "--quiet", "HEAD"]).ok();
    let head = query(&request.child, &["rev-parse", "HEAD"])?;
    let content = git::git_snapshot(&request.child, "refresh-recovery")?.content;
    let reference = pins::reference(&request.namespace, "bases", &request.agent);
    let base = query(&request.child, &["rev-parse", "--verify", &reference]).ok();
    let invalid = if attached.as_deref() != Some(format!("refs/heads/{}", request.branch).as_str())
    {
        Some("branch attachment changed")
    } else if (head != plan.head_old && head != plan.merge_commit)
        || (content != plan.child_content && content != plan.merged_content)
        || (head == plan.merge_commit && content != plan.merged_content)
    {
        Some("child content or branch diverged")
    } else if base.as_deref() != Some(request.base.as_str())
        && base.as_deref() != Some(plan.parent.as_str())
    {
        Some("base pin missing or diverged")
    } else {
        None
    };
    if let Some(reason) = invalid {
        // A fresh dispatch has not written anything. Recovery can prove the
        // same only when both original branch and content survived.
        return if !recover || (head == plan.head_old && content == plan.child_content) {
            Ok(RefreshApplied::Abandoned(format!(
                "refresh precondition failed: {reason}; nothing applied"
            )))
        } else {
            Err(recovery(plan, reason))
        };
    }
    if head == plan.head_old && content == plan.child_content && content != plan.merged_content {
        if recover {
            return Ok(RefreshApplied::Abandoned(
                "refresh stopped before applying; base unchanged".into(),
            ));
        }
        let patch = git::git_run(
            &request.child,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "-M",
                &plan.child_content,
                &plan.merged_content,
            ],
        )
        .map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
        if !patch.status.success() {
            return Err(VcsError::mint_unavailable("cannot construct refresh patch"));
        }
        if let Err(reason) = git::git_apply(&request.child, &patch.stdout) {
            return Ok(RefreshApplied::Abandoned(reason));
        }
    } else if content != plan.merged_content || (head != plan.head_old && head != plan.merge_commit)
    {
        return Err(recovery(plan, "child content or branch diverged"));
    }
    if head == plan.head_old {
        write(
            plan,
            &[
                "update-ref",
                &format!("refs/heads/{}", request.branch),
                &plan.merge_commit,
                &plan.head_old,
            ],
        )?;
    }
    write(plan, &["read-tree", &plan.merge_commit])?;
    if base.as_deref() != Some(plan.parent.as_str()) {
        write(
            plan,
            &["update-ref", &reference, &plan.parent, &request.base],
        )?;
    }
    Ok(RefreshApplied::Refreshed)
}

fn write(plan: &RefreshPlan, args: &[&str]) -> Result<(), VcsError> {
    let output =
        git::git_run(&plan.request.child, args).map_err(|_| recovery(plan, "Git write failed"))?;
    if !output.status.success() {
        return Err(recovery(plan, &git::stderr_excerpt(&output.stderr)));
    }
    Ok(())
}
