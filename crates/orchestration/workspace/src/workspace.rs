//! App-owned workspace presentation facts for the inert terminal view.

use std::path::{Path, PathBuf};

/// `~`-shortened path, plus the checkout's branch or short revision when git
/// can answer for it. Presentation only.
#[must_use]
pub fn workspace_label(cwd: &Path) -> String {
    let path = shorten_home(cwd);
    git_branch(cwd).map_or_else(|| path.clone(), |branch| format!("{path} · {branch}"))
}

fn shorten_home(path: &Path) -> String {
    let display = path.display().to_string();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return display;
    };
    path.strip_prefix(&home).map_or(display, |suffix| {
        if suffix.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", suffix.display())
        }
    })
}

#[cfg(feature = "git-cli")]
fn git_branch(cwd: &Path) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::shorten_home;
    use std::path::PathBuf;

    #[test]
    fn home_prefix_is_presented_compactly() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                shorten_home(&PathBuf::from(home).join("dev/plak")),
                "~/dev/plak"
            );
        }
    }
}

#[cfg(not(feature = "git-cli"))]
fn git_branch(_: &Path) -> Option<String> {
    None
}
