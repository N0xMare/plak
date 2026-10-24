//! Occupancy git-redirect fence for minted child copies.
//!
//! Path tools are already contained by `plakous::tools::WorkspacePolicy`. Process
//! tools (`exec_command`, `run_terminal_command`, `Bash`) start an enrolled shell
//! inside that copy and can still `git -C <parent>` or `cd <parent> && git`.
//! This module installs a basename `git` shim under `{copy}/.plak/bin/git` so
//! the process PATH hits occupancy identity before `/usr/bin/git`.
//!
//! Isolation remains Host: explicit repository access through absolute Git or
//! a deliberately cleared discovery ceiling,
//! `git worktree add` outside the copy, and a hostile shim overwrite are
//! residual, not a VM. Login shells (`/bin/sh -l`) re-export the fence PATH
//! after profile scripts run.
//!
//! The git and jj shims under this module are `format!` templates (`include_str!`)
//! rendered byte-for-byte into the copy. A test compares each render with a frozen copy.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

const FENCE_BIN: &str = ".plak/bin";
const GIT_SHIM_NAME: &str = "git";
const JJ_SHIM_NAME: &str = "jj";

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(test)]
pub(crate) struct GitRedirectDenied {
    pub(crate) detail: String,
}

/// Admit a `git` invocation whose working tree must stay on `allowed_root`.
#[cfg(test)]
pub(crate) fn admit_git_invocation(
    allowed_root: &Path,
    cwd: &Path,
    args: &[impl AsRef<OsStr>],
    git_work_tree: Option<&str>,
    git_dir: Option<&str>,
    git_config_parameters: Option<&str>,
) -> Result<(), GitRedirectDenied> {
    let allowed = canonicalize_or_normalize(allowed_root);
    let parsed = parse_git_globals(cwd, args);
    if parsed.core_worktree || git_config_parameters.is_some_and(assigns_core_worktree) {
        return Err(GitRedirectDenied {
            detail: "core.worktree escapes occupancy copy".into(),
        });
    }
    let work_tree = parsed
        .work_tree
        .as_deref()
        .or(git_work_tree)
        .map(|value| resolve_against(&parsed.cwd, value))
        .unwrap_or_else(|| parsed.cwd.clone());
    if !contained(&allowed, &work_tree) {
        return Err(GitRedirectDenied {
            detail: format!(
                "working tree `{}` escapes occupancy copy `{}`",
                work_tree.display(),
                allowed.display()
            ),
        });
    }
    let Some(git_dir_override) = parsed.git_dir.as_deref().or(git_dir) else {
        return Ok(());
    };
    let requested = resolve_against(&parsed.cwd, git_dir_override);
    if !git_dir_is_this_copy(&allowed, &requested) {
        return Err(GitRedirectDenied {
            detail: format!(
                "git-dir `{}` is not this occupancy copy's git identity",
                requested.display()
            ),
        });
    }
    Ok(())
}

/// True when `path` is under `{launch}/.plak/worktrees/{id}/` or
/// `{launch}/.plak/workspaces/{id}/`.
#[must_use]
pub fn is_minted_occupancy_copy(path: &Path) -> bool {
    path.ancestors().any(|ancestor| {
        ancestor.parent().and_then(Path::file_name) == Some(OsStr::new(".plak"))
            && matches!(
                ancestor.file_name(),
                Some(name) if name == OsStr::new("worktrees") || name == OsStr::new("workspaces")
            )
    })
}

/// True for a minted JJ-copy layout, even if its `.jj` machinery disappeared.
#[must_use]
pub fn is_minted_jj_copy(path: &Path) -> bool {
    path.ancestors().any(|ancestor| {
        ancestor.file_name() == Some(OsStr::new("workspaces"))
            && ancestor.parent().and_then(Path::file_name) == Some(OsStr::new(".plak"))
    })
}

/// Writes `{copy}/.plak/bin/git` so process PATH can intercept basename `git`.
pub(crate) fn install(copy: &Path) -> Result<PathBuf, String> {
    let copy = canonicalize_or_normalize(copy);
    let bin = copy.join(FENCE_BIN);
    std::fs::create_dir_all(&bin).map_err(|error| format!("git fence bin: {error}"))?;
    let shim = bin.join(GIT_SHIM_NAME);
    let script = git_fence_body(&copy);
    write_executable(&shim, &script).map_err(|error| format!("git fence shim: {error}"))?;
    Ok(shim)
}

/// Publishes an executable by writing a sibling temp file and renaming it into
/// place.
///
/// Writing the shim path directly races any concurrent `fork`: the child
/// inherits the still-open write descriptor, and its `exec` of the shim fails
/// `ETXTBSY`. The rename target inode is never open for write, so it cannot.
fn write_executable(path: &Path, body: &str) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("shim")
        .to_owned();
    let temp = path.with_file_name(format!(".{name}.{}.{stamp}.tmp", std::process::id()));
    std::fs::write(&temp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Writes `{copy}/.plak/bin/git` or `{copy}/.plak/bin/jj` for the minted copy kind.
///
/// Jj copies also install the git-redirect shim when `git` is on PATH so
/// colocated `jj git init` checkouts cannot basename-`git` into the parent.
pub fn install_for_copy(copy: &Path, base: Option<&str>) -> Result<PathBuf, String> {
    if is_minted_jj_copy(copy) {
        let base = base.ok_or("jj copy has no recorded tree row; launch from its owning session, or discard or close it and respawn")?;
        super::occupancy_vcs::validate_jj_copy_base(copy, base).map_err(|error| error.detail)?;
        let shim = install_jj(copy, base)?;
        install(copy)?;
        Ok(shim)
    } else {
        install(copy)
    }
}

/// Writes `{copy}/.plak/bin/jj` so process PATH can intercept basename `jj`.
pub(crate) fn install_jj(copy: &Path, base: &str) -> Result<PathBuf, String> {
    if !matches!(base.len(), 40 | 64) || !base.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("jj copy base is not an exact commit identity".into());
    }
    let copy = canonicalize_or_normalize(copy);
    let bin = copy.join(FENCE_BIN);
    std::fs::create_dir_all(&bin).map_err(|error| format!("jj fence bin: {error}"))?;
    let shim = bin.join(JJ_SHIM_NAME);
    let script = jj_fence_body(&copy, base);
    write_executable(&shim, &script).map_err(|error| format!("jj fence shim: {error}"))?;
    Ok(shim)
}

fn git_fence_body(copy: &Path) -> String {
    if is_minted_jj_copy(copy) {
        return jj_copy_git_shim(discover_real_git().ok().as_deref());
    }
    discover_real_git().ok().as_deref().map_or_else(
        || "#!/bin/sh\nprintf 'Git CLI is not installed; native workspace operations remain available\\n' >&2\nexit 127\n".to_owned(),
        |real_git| shim_script(copy, real_git, &own_branch(copy)),
    )
}

fn jj_fence_body(copy: &Path, base: &str) -> String {
    discover_real_jj().ok().as_deref().map_or_else(
        || "#!/bin/sh\nprintf 'JJ CLI is not installed; native workspace operations remain available\\n' >&2\nexit 127\n".to_owned(),
        |real_jj| jj_shim_script(copy, real_jj, base),
    )
}

/// An ignored directory is exempt only if every leaf is an exact minted fence.
/// Never follow links or exempt user content merely because it is under .plak.
pub(crate) fn ignored_path_is_managed(copy: &Path, relative: &Path, base: Option<&str>) -> bool {
    let bin = Path::new(FENCE_BIN);
    let Some(root) = bin.parent() else {
        return false;
    };
    for directory in [root, bin] {
        if !std::fs::symlink_metadata(copy.join(directory)).is_ok_and(|metadata| metadata.is_dir())
        {
            return false;
        }
    }
    if relative == root || relative == bin {
        return std::fs::read_dir(copy.join(relative)).is_ok_and(|entries| {
            entries.into_iter().all(|entry| {
                entry.is_ok_and(|entry| {
                    ignored_path_is_managed(copy, &relative.join(entry.file_name()), base)
                })
            })
        });
    }
    let Ok(canonical) = std::fs::canonicalize(copy) else {
        return false;
    };
    let expected = if relative == bin.join(GIT_SHIM_NAME) {
        git_fence_body(&canonical)
    } else if relative == bin.join(JJ_SHIM_NAME) {
        let Some(base) = base else {
            return false;
        };
        jj_fence_body(&canonical, base)
    } else {
        return false;
    };
    let path = copy.join(relative);
    std::fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected.len() as u64)
        && std::fs::read(path).is_ok_and(|bytes| bytes == expected.as_bytes())
}

/// True when `{root}/.plak/bin/git` or `{root}/.plak/bin/jj` exists so the process
/// backend should prepend that directory to PATH.
#[must_use]
#[cfg(test)]
pub(crate) fn shim_present(root: &Path) -> bool {
    let bin = root.join(FENCE_BIN);
    bin.join(GIT_SHIM_NAME).is_file() || bin.join(JJ_SHIM_NAME).is_file()
}

#[cfg(test)]
struct ParsedGitGlobals {
    cwd: PathBuf,
    work_tree: Option<String>,
    git_dir: Option<String>,
    core_worktree: bool,
}

#[cfg(test)]
fn parse_git_globals(cwd: &Path, args: &[impl AsRef<OsStr>]) -> ParsedGitGlobals {
    let mut cwd = canonicalize_or_normalize(cwd);
    let mut work_tree = None;
    let mut git_dir = None;
    let mut core_worktree = false;
    let mut index = 0;
    if args
        .first()
        .is_some_and(|arg| arg_eq(arg, "git") || path_basename_is_git(arg.as_ref()))
    {
        index = 1;
    }
    while index < args.len() {
        let arg = args[index].as_ref();
        let Some(text) = arg.to_str() else {
            break;
        };
        if text == "--" {
            break;
        }
        if text == "-C" {
            index += 1;
            if let Some(path) = args.get(index).and_then(|value| value.as_ref().to_str()) {
                cwd = resolve_against(&cwd, path);
            }
            index += 1;
            continue;
        }
        if let Some(path) = text.strip_prefix("-C") {
            cwd = resolve_against(&cwd, path);
            index += 1;
            continue;
        }
        if let Some(path) = text.strip_prefix("--work-tree=") {
            work_tree = Some(path.to_owned());
            index += 1;
            continue;
        }
        if text == "--work-tree" {
            index += 1;
            work_tree = args
                .get(index)
                .and_then(|value| value.as_ref().to_str())
                .map(str::to_owned);
            index += 1;
            continue;
        }
        if let Some(path) = text.strip_prefix("--git-dir=") {
            git_dir = Some(path.to_owned());
            index += 1;
            continue;
        }
        if text == "--git-dir" {
            index += 1;
            git_dir = args
                .get(index)
                .and_then(|value| value.as_ref().to_str())
                .map(str::to_owned);
            index += 1;
            continue;
        }
        if text == "-c" || text == "--config-env" {
            index += 1;
            if let Some(value) = args.get(index).and_then(|value| value.as_ref().to_str())
                && assigns_core_worktree(value)
            {
                core_worktree = true;
            }
            index += 1;
            continue;
        }
        if let Some(value) = text.strip_prefix("--config-env=") {
            core_worktree |= assigns_core_worktree(value);
            index += 1;
            continue;
        }
        if matches!(text, "--namespace" | "--attr-source" | "--super-prefix") {
            index += 2;
            continue;
        }
        if let Some(rest) = text.strip_prefix("-c") {
            if assigns_core_worktree(rest.trim_start_matches('=')) {
                core_worktree = true;
            }
            index += 1;
            continue;
        }
        if text.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    ParsedGitGlobals {
        cwd,
        work_tree,
        git_dir,
        core_worktree,
    }
}

#[cfg(test)]
fn path_basename_is_git(arg: &OsStr) -> bool {
    Path::new(arg)
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name == "git" || name == "git.exe")
}

#[cfg(test)]
fn arg_eq(arg: impl AsRef<OsStr>, expected: &str) -> bool {
    arg.as_ref().to_str() == Some(expected)
}

#[cfg(test)]
fn child_git_dir(allowed: &Path) -> Option<PathBuf> {
    let git = allowed.join(".git");
    if git.is_dir() {
        return Some(canonicalize_or_normalize(&git));
    }
    if git.is_file() {
        let body = std::fs::read_to_string(&git).ok()?;
        for line in body.lines() {
            let line = line.trim();
            if let Some(path) = line.strip_prefix("gitdir:") {
                let path = path.trim();
                return Some(resolve_against(allowed, path));
            }
        }
    }
    None
}

#[cfg(test)]
fn git_dir_is_this_copy(allowed: &Path, requested: &Path) -> bool {
    let gitfile = canonicalize_or_normalize(&allowed.join(".git"));
    requested == gitfile || child_git_dir(allowed).is_some_and(|dir| dir == requested)
}

#[cfg(test)]
fn assigns_core_worktree(text: &str) -> bool {
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.to_ascii_lowercase().contains("core.worktree")
}

#[cfg(test)]
fn contained(root: &Path, candidate: &Path) -> bool {
    candidate == root || candidate.starts_with(root)
}

#[cfg(test)]
fn resolve_against(base: &Path, spec: &str) -> PathBuf {
    let path = Path::new(spec);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    canonicalize_or_normalize(&joined)
}

fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

fn discover_real_git() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.ends_with(".plak/bin") {
                continue;
            }
            let candidate = dir.join("git");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    for candidate in [
        "/usr/bin/git",
        "/usr/local/bin/git",
        "/opt/homebrew/bin/git",
        "/bin/git",
    ] {
        let path = Path::new(candidate);
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
    }
    Err("real git executable was not found".into())
}

fn sh_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn discover_real_jj() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("PLAK_TEST_JJ") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.ends_with(".plak/bin") {
                continue;
            }
            let candidate = dir.join("jj");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    for candidate in ["/opt/homebrew/bin/jj", "/usr/local/bin/jj", "/usr/bin/jj"] {
        let path = Path::new(candidate);
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
    }
    Err("real jj executable was not found".into())
}

fn jj_copy_git_shim(real_git: Option<&Path>) -> String {
    format!(
        include_str!("git_redirect_fence/jj_copy_git_shim.sh"),
        real_git = sh_single_quote(
            &real_git
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        )
    )
}

fn jj_shim_script(allowed: &Path, real_jj: &Path, base: &str) -> String {
    format!(
        include_str!("git_redirect_fence/jj_shim.sh"),
        allowed = sh_single_quote(&allowed.display().to_string()),
        real_jj = sh_single_quote(&real_jj.display().to_string()),
        base_config = sh_single_quote(&format!("revset-aliases.\"plak_base()\"=\"{base}\"")),
        immutable_config = sh_single_quote(
            "revset-aliases.\"immutable_heads()\"=\"(all() ~ plak_base()+::) | (working_copies() ~ @)\""
        ),
        snapshot_config = sh_single_quote(&format!(
            "snapshot.auto-track=\"all() ~ ({})\"",
            super::occupancy_vcs::secret_fileset()
        )),
    )
}

/// `plak/<copy directory>` when the mint created that branch, and empty for a
/// detached snapshot. The branch ref is the record, not HEAD, so a child that
/// switches branch still owns the same name. Does not run git.
fn own_branch(copy: &Path) -> String {
    let Some(id) = copy.file_name().and_then(|name| name.to_str()) else {
        return String::new();
    };
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return String::new();
    }
    let minted = format!("plak/{id}");
    if branch_ref_exists(copy, &minted) {
        minted
    } else {
        String::new()
    }
}

fn branch_ref_exists(copy: &Path, branch: &str) -> bool {
    let Some(common) = git_common_dir(copy) else {
        return false;
    };
    if common.join("refs/heads").join(branch).is_file() {
        return true;
    }
    let Ok(packed) = std::fs::read_to_string(common.join("packed-refs")) else {
        return false;
    };
    let needle = format!("refs/heads/{branch}");
    packed.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with('#')
            && !line.starts_with('^')
            && line.split_whitespace().nth(1) == Some(needle.as_str())
    })
}

fn git_common_dir(copy: &Path) -> Option<PathBuf> {
    let git_dir = git_dir_of(copy)?;
    let commondir = git_dir.join("commondir");
    if let Ok(text) = std::fs::read_to_string(&commondir) {
        let raw = text.trim();
        if raw.is_empty() {
            return None;
        }
        let path = Path::new(raw);
        return Some(if path.is_absolute() {
            path.to_path_buf()
        } else {
            git_dir.join(path)
        });
    }
    Some(git_dir)
}

fn git_dir_of(copy: &Path) -> Option<PathBuf> {
    let git = copy.join(".git");
    if git.is_dir() {
        return Some(git);
    }
    let text = std::fs::read_to_string(&git).ok()?;
    let raw = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(raw);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        copy.join(path)
    })
}

fn shim_script(allowed: &Path, real_git: &Path, own_branch: &str) -> String {
    format!(
        include_str!("git_redirect_fence/git_shim.sh"),
        allowed = sh_single_quote(&allowed.display().to_string()),
        real_git = sh_single_quote(&real_git.display().to_string()),
        own_branch = sh_single_quote(own_branch),
    )
}

#[cfg(test)]
mod tests;
