//! Read-only checkout facts; consumers own path and terminal presentation.

use std::path::Path;

/// The checkout's branch or short detached revision, when git can answer.
#[must_use]
#[cfg(feature = "git-cli")]
pub fn workspace_branch(cwd: &Path) -> Option<String> {
    let query = |args: &[&str]| {
        crate::occupancy_vcs::git_query(cwd, args)
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let branch = query(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch.is_empty() {
        None
    } else if branch == "HEAD" {
        query(&["rev-parse", "--short", "HEAD"]).map(|revision| {
            if revision.is_empty() {
                "detached".to_owned()
            } else {
                revision
            }
        })
    } else {
        Some(branch)
    }
}

#[cfg(not(feature = "git-cli"))]
/// No branch fact is available in a build without the Git CLI backend.
#[must_use]
pub fn workspace_branch(_: &Path) -> Option<String> {
    None
}
