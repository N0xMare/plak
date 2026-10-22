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
        |real_git| shim_script(copy, real_git),
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
        if text == "--exec-path" || text == "--namespace" {
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
        r#"#!/bin/sh
REAL_GIT={real_git}
case "$1" in
  --version|version|help)
    [ -n "$REAL_GIT" ] || exit 127
    exec "$REAL_GIT" "$@" ;;
esac
echo "plak: git refused: this copy is a jj workspace with no git directory; git would act on the launch checkout. Use jj (jj st, jj diff, jj log, jj commit). The candidate is sealed from this copy's files." >&2
exit 128
"#,
        real_git = sh_single_quote(
            &real_git
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        )
    )
}

fn jj_shim_script(allowed: &Path, real_jj: &Path, base: &str) -> String {
    format!(
        r#"#!/bin/sh
# Plak occupancy jj-op fence. Basename jj only.
ALLOWED={allowed}
REAL_JJ={real_jj}
refuse() {{
  echo "plak: jj-op refused: $* affects the whole repository, not this copy; report to the parent instead" >&2
  exit 1
}}
check_config() {{
  case "$1" in
    *aliases*|*immutable_heads*|*snapshot*|*\\*) refuse "protected configuration" ;;
  esac
}}
check_track() {{
  # Fileset expressions and directories could include secret files indirectly.
  case "$1" in ''|-*|*[!A-Za-z0-9._/@+,-]*) refuse "file track requires a simple explicit non-secret path" ;; esac
  name=`printf '%s' "${{1##*/}}" | tr '[:upper:]' '[:lower:]'`
  case "$name" in
    .env.example) ;;
    .env|.env.*|.netrc|.npmrc|credentials.json|id_rsa|id_ed25519|*.pem|*.key|*.p12|*.pfx) refuse "secret file track" ;;
  esac
  [ -f "$1" ] || refuse "file track requires explicit non-secret files"
  track_count=$((track_count + 1))
}}
sub1=""
sub2=""
prev=""
at_op=0
track_count=0
global_read=0
arg_index=0
command_index=0
paths_only=0
for arg in "$@"; do
  arg_index=$((arg_index + 1))
  if [ "$paths_only" -eq 1 ] && [ "$sub1" = file ] && [ "$sub2" = track ]; then
    check_track "$arg"
    continue
  fi
  if [ -n "$prev" ]; then
    case "$prev" in
      --config) check_config "$arg" ;;
    esac
    prev=""
    continue
  fi
  # Only -R is a value-taking short global; refuse all clusters/glued values.
  case "$arg" in --*) ;; -??*) refuse "clustered or glued short options; pass values separately" ;; esac
  case "$arg" in
    -R|--repository|-R?*|--repository=*) refuse "repository escapes occupancy copy" ;;
    --ignore-immutable|--ignore-immutable=*) refuse "--ignore-immutable" ;;
    --config-file|--config-file=*) refuse "--config-file" ;;
    --config) prev=$arg ;;
    --config=*) check_config "${{arg#*=}}" ;;
    --at-op|--at-operation) at_op=1; prev=$arg ;;
    --at-op=*|--at-operation=*) at_op=1 ;;
    --color) prev=$arg ;;
    --color=*) ;;
    --help|-h|--version|-V) global_read=1 ;;
    --) paths_only=1 ;;
    -*) if [ "$sub1" = file ] && [ "$sub2" = track ]; then check_track "$arg"; fi ;;
    *)
      if [ -z "$sub1" ]; then sub1=$arg; command_index=$arg_index
      elif [ -z "$sub2" ]; then sub2=$arg
      elif [ "$sub1" = file ] && [ "$sub2" = track ]; then check_track "$arg"
      fi
      ;;
  esac
done
case "$sub1" in
  init) refuse "this occupancy copy is already a workspace of the launch repo" ;;
  undo|redo|gerrit) refuse "$sub1" ;;
  operation|op) case "$sub2" in log|show|diff) ;; *) refuse "$sub1 $sub2" ;; esac ;;
  util) case "$sub2" in completion|config-schema|markdown-help|install-man-pages|exec|snapshot) ;; *) refuse "util $sub2" ;; esac ;;
  git) case "$sub2" in root) ;; *) refuse "this occupancy copy is already a workspace of the launch repo; jj git $sub2" ;; esac ;;
  bookmark|b) case "$sub2" in list|l) ;; *) refuse "bookmark mutation" ;; esac ;;
  tag) case "$sub2" in list|l) ;; *) refuse "tag mutation" ;; esac ;;
  workspace) case "$sub2" in list|root) ;; *) refuse "workspace mutation escapes host mint" ;; esac ;;
  config) case "$sub2" in get|g|list|l|path|p) ;; *) refuse "config mutation" ;; esac ;;
  file) if [ "$sub2" = track ] && [ "$track_count" -eq 0 ]; then refuse "file track requires explicit non-secret files"; fi ;;
  abandon|absorb|arrange|bisect|commit|ci|converge|describe|desc|diff|diffedit|duplicate|edit|evolog|fix|help|interdiff|log|metaedit|new|next|parallelize|prev|rebase|resolve|restore|revert|root|run|show|sign|simplify-parents|sparse|split|squash|status|st|unsign|version) ;;
  "") [ "$global_read" -eq 1 ] || refuse "missing command or configured default alias" ;;
  *) refuse "unknown command or configured alias" ;;
esac
if [ "$at_op" -eq 1 ]; then
  case "$sub1" in
    log|show|diff|status|st|evolog|interdiff) ;;
    file) case "$sub2" in list|show|annotate|search) ;; *) refuse "--at-op file mutation" ;; esac ;;
    operation|op) case "$sub2" in log|show|diff) ;; *) refuse "--at-op operation mutation" ;; esac ;;
    *) refuse "--at-op with mutating command" ;;
  esac
fi

cwd=`pwd -P 2>/dev/null` || refuse "cannot resolve working directory"
case "$cwd" in
  "$ALLOWED"|"$ALLOWED"/*) ;;
  *) refuse "working directory escapes occupancy copy" ;;
esac
[ -d "$ALLOWED/.jj" ] && [ ! -L "$ALLOWED/.jj" ] || refuse "copy .jj is missing or linked"
probe=$cwd
while [ "$probe" != "$ALLOWED" ]; do
  if [ -e "$probe/.jj" ] || [ -L "$probe/.jj" ]; then
    refuse "nested or linked .jj escapes occupancy copy"
  fi
  probe=${{probe%/*}}
done
# Expand admitted default aliases ourselves: jj ignores CLI alias overrides.
expanded=""
case "$sub1" in b) expanded=bookmark ;; ci) expanded=commit ;; desc) expanded=describe ;; op) expanded=operation ;; st) expanded=status ;; esac
if [ -n "$expanded" ]; then
  count=$#
  index=0
  while [ "$index" -lt "$count" ]; do
    arg=$1
    shift
    index=$((index + 1))
    if [ "$index" -eq "$command_index" ]; then set -- "$@" "$expanded"
    else set -- "$@" "$arg"
    fi
  done
fi
exec "$REAL_JJ" --config {base_config} --config {immutable_config} --config {snapshot_config} "$@"
"#,
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

fn shim_script(allowed: &Path, real_git: &Path) -> String {
    format!(
        r#"#!/bin/sh
# Plak occupancy git-redirect fence. Basename git only.
# Parse globals for admission, then exec real git with the original argv.
ALLOWED={allowed}
REAL_GIT={real_git}
cwd=`pwd -P 2>/dev/null || pwd`
worktree=$GIT_WORK_TREE
gitdir=$GIT_DIR
skip=0
prev=""
sub1=""
sub2=""
sub3=""
lc=`printf '%s' "$GIT_CONFIG_PARAMETERS" | tr '[:upper:]' '[:lower:]'`
case "$lc" in
  *core.worktree*)
    echo "plak: git-redirect refused: core.worktree escapes occupancy copy" >&2
    exit 128
    ;;
esac
for arg in "$@"; do
  if [ -n "$sub1" ]; then
    case "$arg" in
      -*) ;;
      *)
        if [ -z "$sub2" ]; then
          sub2=$arg
        elif [ -z "$sub3" ]; then
          sub3=$arg
        fi
        ;;
    esac
    continue
  fi
  if [ "$skip" -eq 1 ]; then
    case "$prev" in
      -C)
        case "$arg" in
          /*) cwd=$arg ;;
          *) cwd=$cwd/$arg ;;
        esac
        cwd=`(cd "$cwd" 2>/dev/null && pwd -P) || echo "$cwd"`
        ;;
      --work-tree) worktree=$arg ;;
      --git-dir) gitdir=$arg ;;
      -c|--config-env)
        lc=`printf '%s' "$arg" | tr '[:upper:]' '[:lower:]'`
        case "$lc" in
          *core.worktree*)
            echo "plak: git-redirect refused: core.worktree escapes occupancy copy" >&2
            exit 128
            ;;
        esac
        ;;
    esac
    skip=0
    prev=""
    continue
  fi
  case "$arg" in
    --) break ;;
    -C|--work-tree|--git-dir|-c|--config-env|--exec-path|--namespace)
      prev=$arg
      skip=1
      ;;
    --work-tree=*)
      worktree=${{arg#--work-tree=}}
      ;;
    --git-dir=*)
      gitdir=${{arg#--git-dir=}}
      ;;
    -*)
      ;;
    *)
      sub1=$arg
      ;;
  esac
done
case "$sub1" in
  init|init-db)
    echo "plak: git-redirect refused: this occupancy copy is already a workspace of the launch repo; 'git init' would make it a standalone repo the launch checkout cannot see" >&2
    exit 128
    ;;
  clone)
    dest=""
    if [ -n "$sub3" ]; then
      dest=$sub3
    fi
    if [ -n "$dest" ]; then
      case "$dest" in
        /*) resolved=$dest ;;
        *) resolved=$cwd/$dest ;;
      esac
      resolved=`(cd "$resolved" 2>/dev/null && pwd -P) || echo "$resolved"`
      if [ "$resolved" = "$ALLOWED" ]; then
        echo "plak: git-redirect refused: this occupancy copy is already a workspace of the launch repo; 'git clone' into the copy root would make it a standalone repo the launch checkout cannot see" >&2
        exit 128
      fi
    fi
    ;;
  worktree)
    if [ "$sub2" = "add" ]; then
      echo "plak: git-redirect refused: this occupancy copy is already a workspace of the launch repo; 'git worktree add' from inside a copy escapes host mint" >&2
      exit 128
    fi
    ;;
esac
target=$cwd
if [ -n "$worktree" ]; then
  case "$worktree" in
    /*) target=$worktree ;;
    *) target=$cwd/$worktree ;;
  esac
fi
target=`(cd "$target" 2>/dev/null && pwd -P) || echo "$target"`
case "$target" in
  "$ALLOWED"|"$ALLOWED"/*) ;;
  *)
    echo "plak: git-redirect refused: working tree escapes occupancy copy" >&2
    exit 128
    ;;
esac
if [ -n "$gitdir" ]; then
  case "$gitdir" in
    /*) requested=$gitdir ;;
    *) requested=$cwd/$gitdir ;;
  esac
  requested=`(cd "$requested" 2>/dev/null && pwd -P) || echo "$requested"`
  expected=""
  if [ -d "$ALLOWED/.git" ]; then
    expected=`(cd "$ALLOWED/.git" && pwd -P)`
  elif [ -f "$ALLOWED/.git" ]; then
    raw=`sed -n 's/^gitdir:[[:space:]]*//p' "$ALLOWED/.git" | head -n 1`
    case "$raw" in
      /*) expected=$raw ;;
      *) expected=$ALLOWED/$raw ;;
    esac
    expected=`(cd "$expected" 2>/dev/null && pwd -P) || echo "$expected"`
  fi
  if [ -z "$expected" ] || {{ [ "$requested" != "$expected" ] && [ "$requested" != "$ALLOWED/.git" ]; }}; then
    echo "plak: git-redirect refused: git-dir is not this occupancy copy" >&2
    exit 128
  fi
fi
exec "$REAL_GIT" "$@"
"#,
        allowed = sh_single_quote(&allowed.display().to_string()),
        real_git = sh_single_quote(&real_git.display().to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        admit_git_invocation, install, install_jj, is_minted_occupancy_copy, shim_present,
    };
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A shim just written by another test thread can still be `ETXTBSY`: a
    /// concurrent `fork` inherits the write descriptor until it execs. Retry
    /// rather than let an unrelated race read as a fence verdict.
    fn output_retrying(command: &mut Command) -> std::process::Output {
        for _ in 0..100 {
            match command.output() {
                Ok(output) => {
                    if String::from_utf8_lossy(&output.stderr).contains("Text file busy") {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    }
                    return output;
                }
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => panic!("shim did not run: {error}"),
            }
        }
        panic!("shim stayed busy");
    }

    fn git_repo() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
        let _ = Command::new("git")
            .args(["-c", "commit.gpgsign=false", "add", "SEED.txt"])
            .current_dir(root.path())
            .status();
        let _ = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.email=plak@local",
                "-c",
                "user.name=plak",
                "commit",
                "--no-gpg-sign",
                "-qm",
                "seed",
            ])
            .current_dir(root.path())
            .status();
        root
    }

    fn mint_child(parent: &Path, id: &str) -> PathBuf {
        let dest = parent.join(".plak/worktrees").join(id);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        assert!(
            Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &format!("plak/{id}"),
                    dest.to_str().unwrap(),
                ])
                .current_dir(parent)
                .status()
                .unwrap()
                .success()
        );
        dest
    }

    #[test]
    fn jj_copy_git_shim_refuses_status_and_never_touches_launch() {
        if !jj_available() {
            return;
        }
        let launch = jj_repo();
        let child = mint_jj_child(launch.path(), "git-fence");
        let shim = install(&child).unwrap();
        std::fs::write(launch.path().join("SEED.txt"), "dirty launch").unwrap();
        // Keep a real launch index to detect accidental Git side effects.
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(launch.path())
                .status()
                .unwrap()
                .success()
        );
        let index = launch.path().join(".git/index");
        let before = std::fs::read(&index).ok();
        std::fs::create_dir(child.join("sub")).unwrap();
        for cwd in [&child, &child.join("sub")] {
            for args in [
                vec!["status"],
                vec!["stash"],
                vec!["checkout", "."],
                vec!["--git-dir=../../../.git", "status"],
            ] {
                let out = Command::new(&shim)
                    .args(&args)
                    .current_dir(cwd)
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(128), "{args:?}");
                assert!(String::from_utf8_lossy(&out.stderr).contains("Use jj"));
            }
        }
        for args in [vec!["--version"], vec!["version"], vec!["help"]] {
            assert!(
                Command::new(&shim)
                    .args(args)
                    .current_dir(&child)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert_eq!(
            std::fs::read_to_string(launch.path().join("SEED.txt")).unwrap(),
            "dirty launch"
        );
        assert_eq!(std::fs::read(&index).ok(), before);
        assert!(super::ignored_path_is_managed(
            &child,
            Path::new(".plak/bin/git"),
            None,
        ));
    }

    #[test]
    fn jj_shim_refuses_nested_or_linked_workspace_discovery() {
        if !jj_available() {
            return;
        }
        let parent = jj_repo();
        let child = mint_jj_child(parent.path(), "discovery-child");
        let sibling = mint_jj_child(parent.path(), "discovery-sibling");
        let sibling_head = jj(
            &sibling,
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
        )
        .stdout;
        let base = String::from_utf8(
            jj(
                &child,
                &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            )
            .stdout,
        )
        .unwrap();
        let shim = install_jj(&child, base.trim()).unwrap();
        let sub = child.join("sub");
        let deep = sub.join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        let run = |cwd: &Path, args: &[&str]| {
            Command::new(&shim)
                .args(args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(cwd)
                .output()
                .unwrap()
        };
        assert!(run(&deep, &["status"]).status.success());
        let refuse_new = |cwd: &Path| {
            let output = run(cwd, &["new", "root()"]);
            assert_eq!(output.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&output.stderr).contains("plak: jj-op refused"));
            assert_eq!(
                jj(
                    &sibling,
                    &["log", "-r", "@", "--no-graph", "-T", "commit_id"]
                )
                .stdout,
                sibling_head
            );
        };
        let nested = sub.join(".jj");
        std::os::unix::fs::symlink(sibling.join(".jj"), &nested).unwrap();
        refuse_new(&sub);
        refuse_new(&deep);
        std::fs::remove_file(&nested).unwrap();
        std::os::unix::fs::symlink(sub.join("missing"), &nested).unwrap();
        refuse_new(&deep);
        std::fs::remove_file(&nested).unwrap();
        std::fs::create_dir(&nested).unwrap();
        refuse_new(&deep);
        std::fs::remove_dir(&nested).unwrap();

        let marker = child.join(".jj");
        let saved = child.join("saved-jj");
        std::fs::rename(&marker, &saved).unwrap();
        refuse_new(&child);
        std::os::unix::fs::symlink(sibling.join(".jj"), &marker).unwrap();
        refuse_new(&child);
        std::fs::remove_file(&marker).unwrap();
        std::fs::rename(&saved, &marker).unwrap();
        assert!(run(&deep, &["status"]).status.success());
    }

    #[test]
    fn jj_shim_refuses_repository_wide_operations() {
        if !jj_available() {
            return;
        }
        let parent = jj_repo();
        let child = mint_jj_child(parent.path(), "refusals");
        let sibling = mint_jj_child(parent.path(), "cluster-sibling");
        std::fs::write(sibling.join("keep.txt"), "sibling work").unwrap();
        let sibling_head = jj(
            &sibling,
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
        )
        .stdout;
        let launch_head = jj(
            parent.path(),
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
        )
        .stdout;
        let base = String::from_utf8(
            jj(
                &child,
                &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            )
            .stdout,
        )
        .unwrap();
        let shim = install_jj(&child, base.trim()).unwrap();
        let before = jj(
            parent.path(),
            &["op", "log", "--no-graph", "-n", "1", "-T", "id"],
        )
        .stdout;
        let glued = format!("-sR{}", sibling.display());
        let glued_log = format!("-GR{}", parent.path().display());
        let cases = [
            vec!["log", "-GR", parent.path().to_str().unwrap()],
            vec!["log", &glued_log],
            vec!["diff", &glued],
            vec!["prev", "-nR", sibling.to_str().unwrap()],
            vec!["next", "-nR", parent.path().to_str().unwrap()],
            vec!["--color", "never", "--at-op", "@", "undo"],
            vec!["log", "--config-file=ignored"],
            vec!["workspace", "remove", "plak-sibling"],
            vec!["tag", "track", "x@origin"],
            vec!["tag", "untrack", "x@origin"],
            vec!["dangerous-alias"],
            vec!["util", "backend"],
            vec!["--at-op", "@", "file", "track", ".env"],
            vec!["--at-op", "@", "file", "untrack", "file.txt"],
            vec!["--at-op", "@", "op", "integrate"],
            vec!["file", "track", ".env"],
            vec!["file", "track", "sub/ID_RSA"],
            vec!["file", "track", "root-glob:**"],
            vec!["file", "track"],
            vec!["undo"],
            vec!["redo"],
            vec!["--color", "never", "undo"],
            vec!["--color=never", "redo"],
            vec!["op", "restore", "@-"],
            vec!["operation", "revert", "@-"],
            vec!["op", "abandon", "@-"],
            vec!["op", "integrate"],
            vec!["util", "gc"],
            vec!["git", "push"],
            vec!["git", "fetch"],
            vec!["git", "import"],
            vec!["git", "export"],
            vec!["git", "remote", "list"],
            vec!["bookmark", "set", "main"],
            vec!["b", "s", "main"],
            vec!["bookmark", "delete", "main"],
            vec!["tag", "set", "test"],
            vec!["tag", "s", "test"],
            vec!["tag", "delete", "test"],
            vec!["tag", "d", "test"],
            vec!["workspace", "add", "elsewhere"],
            vec!["workspace", "forget"],
            vec!["workspace", "rename", "other"],
            vec!["workspace", "update-stale"],
            vec!["config", "set", "user.name", "other"],
            vec!["config", "s"],
            vec!["config", "unset"],
            vec!["config", "u"],
            vec!["config", "edit"],
            vec!["config", "e"],
            vec!["config", "gc"],
            vec!["gerrit"],
            vec!["--ignore-immutable", "describe"],
            vec!["--config-file", "config.toml", "status"],
            vec!["--config-file=config.toml", "status"],
            vec!["--config", "revset-aliases.x='all()'", "status"],
            vec!["--config=revset-aliases.x='all()'", "status"],
            vec!["--config", "snapshot.auto-track='all()'", "status"],
            vec!["--at-op", "@-", "describe"],
            vec!["--at-operation=@-", "commit"],
            vec!["-R", ".", "status"],
            vec!["--repository=.", "status"],
        ];
        for args in cases {
            let out = Command::new(&shim)
                .args(&args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&child)
                .output()
                .unwrap();
            assert_eq!(
                out.status.code(),
                Some(1),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stderr).contains("jj-op refused"));
        }
        assert_eq!(
            jj(
                parent.path(),
                &["op", "log", "--no-graph", "-n", "1", "-T", "id"]
            )
            .stdout,
            before
        );
        assert_eq!(
            std::fs::read_to_string(sibling.join("keep.txt")).unwrap(),
            "sibling work"
        );
        assert_eq!(
            jj(
                &sibling,
                &["log", "-r", "@", "--no-graph", "-T", "commit_id"]
            )
            .stdout,
            sibling_head
        );
        assert_eq!(
            jj(
                parent.path(),
                &["log", "-r", "@", "--no-graph", "-T", "commit_id"]
            )
            .stdout,
            launch_head
        );
        assert!(super::ignored_path_is_managed(
            &child,
            Path::new(".plak/bin/jj"),
            Some(base.trim())
        ));
        assert!(!super::ignored_path_is_managed(
            &child,
            Path::new(".plak/bin/jj"),
            Some("wrong-base")
        ));
    }

    #[cfg(feature = "jj-lib")]
    #[test]
    fn jj_shim_immutability_and_secret_auto_track() {
        if !jj_available() {
            return;
        }
        let launch = crate::vcs_fixtures::jj_repo();
        let child =
            crate::native_jj::mint(launch.path(), "protected", launch.path(), None).unwrap();
        let sibling =
            crate::native_jj::mint(launch.path(), "sibling", launch.path(), None).unwrap();
        let shim = child.path.join(".plak/bin/jj");
        let before = jj(
            launch.path(),
            &["op", "log", "--no-graph", "-n", "1", "-T", "id"],
        )
        .stdout;
        for args in [
            vec!["workspace", "remove", sibling.branch.as_deref().unwrap()],
            vec!["tag", "track", "x@origin"],
        ] {
            let denied = Command::new(&shim)
                .args(args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&child.path)
                .output()
                .unwrap();
            assert_eq!(denied.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&denied.stderr).contains("jj-op refused"));
        }
        assert!(sibling.path.join("SEED.txt").is_file());
        assert_eq!(
            jj(
                launch.path(),
                &["op", "log", "--no-graph", "-n", "1", "-T", "id"]
            )
            .stdout,
            before
        );
        let config = launch.path().join("aliases.toml");
        std::fs::write(&config, "[aliases]\ndangerous-alias=['undo']\nb=['undo']\n").unwrap();
        let denied = Command::new(&shim)
            .arg("dangerous-alias")
            .env("JJ_CONFIG", &config)
            .current_dir(&child.path)
            .output()
            .unwrap();
        assert_eq!(denied.status.code(), Some(1));
        let safe = Command::new(&shim)
            .args(["b", "list"])
            .env("JJ_CONFIG", &config)
            .current_dir(&child.path)
            .output()
            .unwrap();
        assert!(
            safe.status.success(),
            "{}",
            String::from_utf8_lossy(&safe.stderr)
        );
        std::fs::remove_file(config).unwrap();
        let root = String::from_utf8(
            jj(
                launch.path(),
                &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            )
            .stdout,
        )
        .unwrap();
        let sibling_head = String::from_utf8(
            jj(
                &sibling.path,
                &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            )
            .stdout,
        )
        .unwrap();
        for revision in [root.trim(), sibling_head.trim(), child.base.commit.as_str()] {
            for command in ["edit", "abandon", "describe"] {
                let mut args = vec![command, "-r", revision];
                if command == "describe" {
                    args.extend(["-m", "forbidden"]);
                }
                let out = Command::new(&shim)
                    .args(&args)
                    .env("JJ_CONFIG", "/dev/null")
                    .current_dir(&child.path)
                    .output()
                    .unwrap();
                assert!(!out.status.success(), "{args:?}");
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains("immutable"),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
        std::fs::create_dir(child.path.join("sub")).unwrap();
        for name in [
            ".env",
            ".ENV.production",
            "id_rsa",
            "x.pem",
            "-id.pem",
            "sub/X.KEY",
            ".env.example",
            "ordinary.txt",
        ] {
            std::fs::write(child.path.join(name), "fixture").unwrap();
        }
        for args in [
            vec!["file", "track", ".env"],
            vec!["file", "track", "\".env\""],
            vec!["file", "track", "ordinary.txt&.env"],
            vec!["file", "track", "ordinary.txt", "--", "-id.pem"],
            vec!["file", "track", "all()"],
            vec!["--at-op", "@", "file", "track", "ordinary.txt"],
        ] {
            let denied = Command::new(&shim)
                .args(args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&child.path)
                .output()
                .unwrap();
            assert_eq!(denied.status.code(), Some(1));
        }
        let tracked = Command::new(&shim)
            .args(["file", "track", "ordinary.txt", ".env.example"])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(&child.path)
            .output()
            .unwrap();
        assert!(
            tracked.status.success(),
            "{}",
            String::from_utf8_lossy(&tracked.stderr)
        );
        for args in [
            vec!["st"],
            vec!["log"],
            vec!["diff", "--from", "plak_base()"],
            vec!["op", "log"],
            vec!["bookmark", "list"],
            vec!["b", "l"],
            vec!["git", "root"],
            vec!["describe", "-m", "own"],
            vec!["commit", "-m", "own"],
            vec!["new"],
        ] {
            let out = Command::new(&shim)
                .args(&args)
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(child.path.join("sub"))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let output = Command::new(&shim)
            .args(["file", "list"])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(&child.path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let listed = String::from_utf8(output.stdout).unwrap();
        for name in [
            ".env",
            ".ENV.production",
            "id_rsa",
            "x.pem",
            "-id.pem",
            "sub/X.KEY",
        ] {
            assert!(!listed.lines().any(|line| line == name), "{listed}");
        }
        assert!(listed.contains(".env.example"));
        crate::native_jj::snapshot(&child.path).unwrap();
    }
    #[test]
    fn cwd_inside_copy_is_admitted() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        admit_git_invocation(&child, &child, &["git", "status"], None, None, None).unwrap();
        admit_git_invocation(
            &child,
            &child,
            &["git", "-C", ".", "status"],
            None,
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn minus_c_parent_is_denied() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let error = admit_git_invocation(
            &child,
            &child,
            &["git", "-C", parent.path().to_str().unwrap(), "status"],
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.detail.contains("escapes occupancy copy"));
    }

    #[test]
    fn work_tree_parent_is_denied() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let parent_s = parent.path().to_str().unwrap();
        assert!(
            admit_git_invocation(
                &child,
                &child,
                &["git", &format!("--work-tree={parent_s}"), "status"],
                None,
                None,
                None,
            )
            .is_err()
        );
        assert!(
            admit_git_invocation(
                &child,
                &child,
                &["git", "--work-tree", parent_s, "status"],
                None,
                None,
                None,
            )
            .is_err()
        );
        assert!(
            admit_git_invocation(
                &child,
                &child,
                &["git", "status"],
                Some(parent_s),
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn git_dir_parent_is_denied() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let parent_git = parent.path().join(".git");
        let parent_git_s = parent_git.to_str().unwrap();
        assert!(
            admit_git_invocation(
                &child,
                &child,
                &["git", &format!("--git-dir={parent_git_s}"), "status"],
                None,
                None,
                None,
            )
            .is_err()
        );
        assert!(
            admit_git_invocation(
                &child,
                &child,
                &["git", "status"],
                None,
                Some(parent_git_s),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn sibling_worktree_is_denied() {
        let parent = git_repo();
        let c1 = mint_child(parent.path(), "c1");
        let c2 = mint_child(parent.path(), "c2");
        assert!(
            admit_git_invocation(
                &c1,
                &c1,
                &["git", "-C", c2.to_str().unwrap(), "status"],
                None,
                None,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn chained_minus_c_into_parent_is_denied() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        assert!(
            admit_git_invocation(
                &child,
                &child,
                &["git", "-C", "..", "-C", "..", "status"],
                None,
                None,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn installed_shim_refuses_parent_and_allows_copy() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let shim = install(&child).unwrap();
        assert!(shim_present(&child));
        let denied = output_retrying(
            Command::new(&shim)
                .args(["-C", parent.path().to_str().unwrap(), "status"])
                .current_dir(&child),
        );
        assert!(!denied.status.success());
        let stderr = String::from_utf8_lossy(&denied.stderr);
        assert!(stderr.contains("git-redirect refused"));
        let allowed = output_retrying(
            Command::new(&shim)
                .args(["status", "--porcelain"])
                .current_dir(&child),
        );
        assert!(
            allowed.status.success(),
            "{}",
            String::from_utf8_lossy(&allowed.stderr)
        );
    }

    #[test]
    fn installed_shim_refuses_work_tree_env() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let shim = install(&child).unwrap();
        let denied = output_retrying(
            Command::new(&shim)
                .env("GIT_WORK_TREE", parent.path())
                .args(["status"])
                .current_dir(&child),
        );
        assert!(!denied.status.success());
    }

    #[test]
    fn shell_path_hit_refuses_git_dash_c_parent() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let shim = install(&child).unwrap();
        let bin = shim.parent().unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let denied = output_retrying(
            Command::new("/bin/sh")
                .args(["-c", &format!("git -C {} status", parent.path().display())])
                .env("PATH", path)
                .current_dir(&child),
        );
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("git-redirect refused"));
    }

    #[test]
    fn core_worktree_config_is_denied() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let parent_s = parent.path().to_str().unwrap();
        assert!(
            admit_git_invocation(
                &child,
                &child,
                &["git", "-c", &format!("core.worktree={parent_s}"), "status"],
                None,
                None,
                None,
            )
            .is_err()
        );
        assert!(
            admit_git_invocation(
                &child,
                &child,
                &["git", "status"],
                None,
                None,
                Some(&format!("core.worktree={parent_s}")),
            )
            .is_err()
        );
        let shim = install(&child).unwrap();
        let denied = output_retrying(
            Command::new(&shim)
                .args(["-c", &format!("core.worktree={parent_s}"), "status"])
                .current_dir(&child),
        );
        assert!(!denied.status.success());
    }

    #[test]
    fn gitfile_git_dir_is_admitted() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        admit_git_invocation(
            &child,
            &child,
            &["git", "--git-dir=.git", "status"],
            None,
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn minted_copy_predicate_is_layout_not_path_inequality() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        assert!(is_minted_occupancy_copy(&child));
        assert!(!is_minted_occupancy_copy(parent.path()));
        assert!(!is_minted_occupancy_copy(&parent.path().join("src")));
    }

    #[test]
    fn installed_shim_refuses_git_init_inside_copy() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let shim = install(&child).unwrap();
        for args in [
            vec!["init"],
            vec!["init", "."],
            vec!["-c", "init.defaultBranch=main", "init"],
        ] {
            let denied = output_retrying(Command::new(&shim).args(&args).current_dir(&child));
            assert!(!denied.status.success(), "{args:?} was admitted");
            let stderr = String::from_utf8_lossy(&denied.stderr);
            assert!(
                stderr.contains("already a workspace of the launch repo"),
                "{stderr}"
            );
        }
        assert!(!child.join(".git").is_dir());
    }

    #[test]
    fn installed_shim_refuses_clone_into_copy_root_but_allows_subdir_clone_args() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let shim = install(&child).unwrap();
        let denied = output_retrying(
            Command::new(&shim)
                .args(["clone", parent.path().to_str().unwrap(), "."])
                .current_dir(&child),
        );
        assert!(!denied.status.success());
        assert!(
            String::from_utf8_lossy(&denied.stderr)
                .contains("already a workspace of the launch repo")
        );
        let nested = output_retrying(
            Command::new(&shim)
                .args(["clone", parent.path().to_str().unwrap(), "vendor"])
                .current_dir(&child),
        );
        assert!(
            !String::from_utf8_lossy(&nested.stderr).contains("git-redirect refused"),
            "{}",
            String::from_utf8_lossy(&nested.stderr)
        );
    }

    #[test]
    fn installed_shim_refuses_worktree_add_from_inside_copy() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let shim = install(&child).unwrap();
        let denied = output_retrying(
            Command::new(&shim)
                .args(["worktree", "add", "sub"])
                .current_dir(&child),
        );
        assert!(!denied.status.success());
        assert!(
            String::from_utf8_lossy(&denied.stderr)
                .contains("already a workspace of the launch repo")
        );
        let listed = output_retrying(
            Command::new(&shim)
                .args(["worktree", "list"])
                .current_dir(&child),
        );
        assert!(
            listed.status.success(),
            "{}",
            String::from_utf8_lossy(&listed.stderr)
        );
    }

    #[test]
    fn installed_shim_admits_subcommand_flags_that_look_like_globals() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let shim = install(&child).unwrap();
        let listed = output_retrying(
            Command::new(&shim)
                .args(["log", "-1", "--format=%s", "-C"])
                .current_dir(&child),
        );
        assert!(
            listed.status.success(),
            "{}",
            String::from_utf8_lossy(&listed.stderr)
        );
    }

    fn jj_available() -> bool {
        Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn jj(root: &Path, args: &[&str]) -> std::process::Output {
        Command::new(std::env::var_os("PLAK_TEST_JJ").unwrap_or_else(|| "jj".into()))
            .args(["--no-pager"])
            .args([
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local",
            ])
            .args(args)
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(root)
            .output()
            .unwrap()
    }

    fn jj_repo() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        assert!(jj(root.path(), &["git", "init"]).status.success());
        std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
        assert!(jj(root.path(), &["commit", "-m", "seed"]).status.success());
        root
    }

    fn mint_jj_child(parent: &Path, id: &str) -> PathBuf {
        let dest = parent.join(".plak/workspaces").join(id);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let out = jj(
            parent,
            &["workspace", "add", "--name", id, dest.to_str().unwrap()],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        dest
    }

    /// A child ran `jj git init` inside its copy and turned
    /// it into a standalone repo the launch checkout could not see.
    #[test]
    fn installed_jj_shim_refuses_repo_creation_inside_copy() {
        if !jj_available() {
            return;
        }
        let parent = jj_repo();
        let child = mint_jj_child(parent.path(), "c1");
        let base = String::from_utf8(
            jj(
                &child,
                &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            )
            .stdout,
        )
        .unwrap();
        let shim = install_jj(&child, base.trim()).unwrap();
        for args in [
            vec!["init"],
            vec!["git", "init"],
            vec!["git", "init", "--colocate"],
            vec!["git", "clone", "https://example.invalid/x.git"],
        ] {
            let denied = output_retrying(
                Command::new(&shim)
                    .args(&args)
                    .env("JJ_CONFIG", "/dev/null")
                    .current_dir(&child),
            );
            assert!(!denied.status.success(), "{args:?} was admitted");
            let stderr = String::from_utf8_lossy(&denied.stderr);
            assert!(
                stderr.contains("jj-op refused")
                    && stderr.contains("already a workspace of the launch repo"),
                "{args:?}: {stderr}"
            );
        }
        assert!(
            child.join(".jj").join("repo").is_file(),
            "copy stopped being a workspace pointer of the launch repo"
        );
    }

    #[test]
    fn installed_jj_shim_still_refuses_workspace_mutation_and_parent_repo() {
        if !jj_available() {
            return;
        }
        let parent = jj_repo();
        let child = mint_jj_child(parent.path(), "c1");
        let base = String::from_utf8(
            jj(
                &child,
                &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            )
            .stdout,
        )
        .unwrap();
        let shim = install_jj(&child, base.trim()).unwrap();
        for args in [
            vec!["workspace", "add", "sub"],
            vec!["workspace", "forget", "c1"],
            vec!["workspace", "rename", "c2"],
        ] {
            let denied = output_retrying(
                Command::new(&shim)
                    .args(&args)
                    .env("JJ_CONFIG", "/dev/null")
                    .current_dir(&child),
            );
            assert!(!denied.status.success(), "{args:?} was admitted");
            assert!(
                String::from_utf8_lossy(&denied.stderr)
                    .contains("workspace mutation escapes host mint")
            );
        }
        let denied = output_retrying(
            Command::new(&shim)
                .args(["-R", parent.path().to_str().unwrap(), "status"])
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(&child),
        );
        assert!(!denied.status.success());
        assert!(
            String::from_utf8_lossy(&denied.stderr).contains("repository escapes occupancy copy")
        );
    }

    #[test]
    fn installed_jj_shim_admits_ordinary_commands_in_the_copy() {
        if !jj_available() {
            return;
        }
        let parent = jj_repo();
        let child = mint_jj_child(parent.path(), "c1");
        let base = String::from_utf8(
            jj(
                &child,
                &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            )
            .stdout,
        )
        .unwrap();
        let shim = install_jj(&child, base.trim()).unwrap();
        for args in [
            vec!["status"],
            vec!["workspace", "root"],
            vec!["describe", "-m", "init"],
        ] {
            let out = output_retrying(
                Command::new(&shim)
                    .args(&args)
                    .env("JJ_CONFIG", "/dev/null")
                    .current_dir(&child),
            );
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn login_shell_still_hits_shim_after_path_restore() {
        let parent = git_repo();
        let child = mint_child(parent.path(), "c1");
        let shim = install(&child).unwrap();
        let bin = shim.parent().unwrap();
        let wrapped = format!(
            "PATH={}:\"$PATH\"; export PATH\ngit -C {} status",
            bin.display(),
            parent.path().display()
        );
        let denied = output_retrying(
            Command::new("/bin/sh")
                .args(["-l", "-c", &wrapped])
                .env("PATH", "/usr/bin:/bin")
                .current_dir(&child),
        );
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("git-redirect refused"));
    }
}
