//! Explicit maintenance over an owner's recorded physical artifact names.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

pub use super::{candidate_ref, digest_text};

use super::{VcsError, git_query, git_run};

/// A normal seal omits secrets and ignored content. Never delete those bytes
/// merely because a checkout is idle. Only exact minted fences and the local
/// build cache are disposable; uncertainty retains the entire checkout.
pub fn check_reclaimable(path: &Path, sealed: Option<&super::VcsSnapshot>) -> Result<(), VcsError> {
    if super::detect_vcs(path)? != super::SessionVcs::Git {
        return Err(VcsError::mint_unavailable(
            "workspace coverage inspection unavailable for JJ",
        ));
    }
    let index = git(path, &["ls-files", "--stage", "-z"])?;
    if index
        .split(|byte| *byte == 0)
        .any(|entry| entry.starts_with(b"160000 "))
    {
        return Err(VcsError::mint_unavailable(
            "submodule contents are not covered by normal sealing; workspace retained",
        ));
    }
    let names = git(
        path,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let text = String::from_utf8_lossy(name);
        if text.ends_with('/') || super::is_secret_git_path(&text) {
            return Err(VcsError::mint_unavailable(
                "secret content is not covered by normal sealing; workspace retained",
            ));
        }
    }
    let ignored = git(
        path,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
        ],
    )?;
    for name in ignored
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let relative = Path::new(std::ffi::OsStr::from_bytes(name));
        if super::is_secret_git_path(&String::from_utf8_lossy(name)) {
            return Err(VcsError::mint_unavailable(
                "ignored secret content is not covered by normal sealing; workspace retained",
            ));
        }
        if relative.starts_with("target")
            || crate::git_redirect_fence::ignored_path_is_managed(path, relative)
        {
            continue;
        }
        return Err(VcsError::mint_unavailable(
            "ignored content is not covered by normal sealing; workspace retained",
        ));
    }
    // A staged version that differs from the working tree is independent work.
    // A normal seal captures only the working tree, not that second version.
    let staged = git(path, &["diff", "--cached", "--name-only", "-z"])?;
    let unstaged = git(path, &["diff", "--name-only", "-z"])?;
    if staged
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .any(|name| unstaged.split(|byte| *byte == 0).any(|other| other == name))
    {
        return Err(VcsError::mint_unavailable(
            "independent staged content is not covered by normal sealing; workspace retained",
        ));
    }
    if let Some(sealed) = sealed
        && super::snapshot(path, "reclaim-check")?.content != sealed.content
    {
        return Err(VcsError::mint_unavailable(
            "workspace changed after sealing; workspace retained",
        ));
    }
    Ok(())
}

/// A read-only changes probe; callers retain unresolved work on probe failure.
pub fn dirty(path: &Path) -> Result<bool, VcsError> {
    match super::detect_vcs(path)? {
        // The ordinary JJ occupancy probe snapshots and publishes its working
        // copy. Export is read-only, so uncertainty stays an inert reference.
        super::SessionVcs::Jj => Err(VcsError::mint_unavailable(
            "read-only JJ changes inspection unavailable",
        )),
        super::SessionVcs::Git => {
            Ok(!git(path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty())
        }
    }
}

fn git(launch: &Path, args: &[&str]) -> Result<Vec<u8>, VcsError> {
    let output =
        git_query(launch, args).map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
    if !output.status.success() {
        return Err(VcsError::mint_unavailable(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output.stdout)
}

/// Whether a recorded candidate pin still exists; explicit discard may have
/// removed it without removing the historical candidate row.
pub fn reference_exists(launch: &Path, reference: &str) -> Result<bool, VcsError> {
    let output = git_query(launch, &["show-ref", "--verify", "--quiet", reference])
        .map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
    if output.status.success() {
        Ok(true)
    } else if output.status.code() == Some(1) {
        Ok(false)
    } else {
        Err(VcsError::mint_unavailable("cannot inspect owned reference"))
    }
}

/// Validates exact registered worktrees and refs; returns whether each copy
/// has uncommitted changes. No repository mutation occurs here.
pub fn inspect(
    launch: &Path,
    worktrees: &[PathBuf],
    references: &[String],
) -> Result<Vec<bool>, VcsError> {
    if matches!(super::detect_vcs(launch), Ok(super::SessionVcs::Jj))
        || worktrees.iter().any(|path| path.join(".jj").exists())
    {
        return Err(VcsError::mint_unavailable(
            "JJ artifacts retained: read-only ownership and reachability inspection unavailable",
        ));
    }
    let registrations = git(launch, &["worktree", "list", "--porcelain", "-z"])?;
    let launch_head = git(launch, &["rev-parse", "HEAD"])?;
    let launch_head = String::from_utf8_lossy(&launch_head).trim().to_owned();
    let mut dirty = Vec::new();
    for path in worktrees {
        let parent = path
            .parent()
            .ok_or_else(|| VcsError::mint_unavailable("invalid worktree path"))?;
        if parent != launch.join(".plak/worktrees") && parent != launch.join(".plak/workspaces") {
            return Err(VcsError::mint_unavailable(
                "refusing worktree outside its recorded launch repository",
            ));
        }
        for ancestor in [launch.join(".plak"), parent.to_path_buf(), path.clone()] {
            match std::fs::symlink_metadata(&ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(VcsError::mint_unavailable("refusing linked worktree path"));
                }
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(VcsError::mint_unavailable(error.to_string()));
                }
                _ => {}
            }
        }
        let registered = registrations
            .split(|byte| *byte == 0)
            .any(|field| field == format!("worktree {}", path.display()).as_bytes());
        if path.exists() && !registered {
            return Err(VcsError::mint_unavailable(format!(
                "unregistered directory retained: {}",
                path.display()
            )));
        }
        dirty.push(if path.exists() {
            let count = git(
                path,
                &["rev-list", "--count", "HEAD", "--not", &launch_head],
            )?;
            if String::from_utf8_lossy(&count).trim() != "0" {
                return Err(VcsError::mint_unavailable(format!(
                    "UNADOPTED COMMITS; worktree retained: {}",
                    path.display()
                )));
            }
            let status = git(
                path,
                &[
                    "status",
                    "--porcelain",
                    "-z",
                    "--untracked-files=all",
                    "--ignored=matching",
                ],
            )?;
            let mut changed = false;
            for entry in status
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
            {
                if let Some(ignored) = entry.strip_prefix(b"!! ") {
                    let relative = Path::new(std::ffi::OsStr::from_bytes(ignored));
                    if !crate::git_redirect_fence::ignored_path_is_managed(path, relative) {
                        return Err(VcsError::mint_unavailable(format!(
                            "ignored files; worktree retained: {} ({})",
                            path.display(),
                            relative.display()
                        )));
                    }
                } else {
                    changed = true;
                }
            }
            changed
        } else {
            false
        });
    }
    for reference in references {
        if !reference.starts_with("refs/heads/plak/") && !reference.starts_with("refs/plak/") {
            return Err(VcsError::mint_unavailable("refusing non-Plak reference"));
        }
        git(launch, &["check-ref-format", reference])?;
        // A clean status says nothing about committed but unlanded work.
        let exists = git_query(launch, &["show-ref", "--verify", "--quiet", reference])
            .map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
        if exists.status.success() {
            let count = git(launch, &["rev-list", "--count", reference, "--not", "HEAD"])?;
            if String::from_utf8_lossy(&count).trim() != "0" {
                return Err(VcsError::mint_unavailable(format!(
                    "UNADOPTED COMMITS; reference retained: {reference}"
                )));
            }
        } else if exists.status.code() != Some(1) {
            return Err(VcsError::mint_unavailable("cannot inspect owned reference"));
        }

        let mut checkout = None;
        for field in registrations.split(|byte| *byte == 0) {
            if let Some(path) = field.strip_prefix(b"worktree ") {
                checkout = Some(String::from_utf8_lossy(path).into_owned());
            }
            if field == format!("branch {reference}").as_bytes()
                && checkout.as_ref().is_some_and(|checkout| {
                    !worktrees
                        .iter()
                        .any(|path| path.to_string_lossy() == checkout.as_str())
                })
            {
                return Err(VcsError::mint_unavailable(format!(
                    "reference is checked out outside the selected worktrees; retained: {reference}"
                )));
            }
        }
    }
    Ok(dirty)
}

/// Removes only the supplied owned artifacts. Dirty copies are retained even
/// with explicit confirmation; every copy is checked before the first write.
pub fn remove(launch: &Path, worktrees: &[PathBuf], references: &[String]) -> Result<(), VcsError> {
    let dirty = inspect(launch, worktrees, references)?;
    if let Some((path, _)) = worktrees.iter().zip(dirty).find(|(_, dirty)| *dirty) {
        return Err(VcsError::mint_unavailable(format!(
            "uncommitted changes; worktree retained: {}",
            path.display()
        )));
    }
    let registrations = git(launch, &["worktree", "list", "--porcelain", "-z"])?;
    for path in worktrees {
        if registrations
            .split(|byte| *byte == 0)
            .any(|field| field == format!("worktree {}", path.display()).as_bytes())
        {
            // Maintenance never overrides Git's final refusal (e.g. submodules
            // or changes arriving after inspection). Explicit discard is separate.
            let output = git_run(launch, &["worktree", "remove", &path.display().to_string()])
                .map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
            if !output.status.success() {
                return Err(VcsError::mint_unavailable(format!(
                    "worktree removal refused; retained: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }
    }
    for reference in references {
        let output = git_run(launch, &["update-ref", "-d", reference])
            .map_err(|error| VcsError::mint_unavailable(error.to_string()))?;
        if !output.status.success() {
            return Err(VcsError::mint_unavailable(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
    }
    let common = git(
        launch,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(String::from_utf8_lossy(&common).trim());
    for reference in references {
        let Some(prefix) = reference
            .strip_prefix("refs/plak/")
            .and_then(|rest| rest.split('/').next())
        else {
            continue;
        };
        if prefix.len() == 16 && prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            super::namespace::cleanup(&common, prefix)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn read_only_jj_probe_refuses_without_opening_or_snapshotting() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".jj")).unwrap();
        let error = super::dirty(root.path()).unwrap_err();
        assert!(
            error
                .detail
                .contains("read-only JJ changes inspection unavailable")
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
