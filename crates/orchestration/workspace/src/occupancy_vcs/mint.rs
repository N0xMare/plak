//! Copy minting and the bounded on-mint hook.

use std::process::Command;

use super::{
    BTreeMap, MintedCopy, Path, VcsError, VcsSnapshot, WorkspaceMode, create_private,
    ensure_plak_mint_excludes, git_run, git_snapshot, jj_mint_lock, mint_error, private_scratch,
    reclaim_orphan_worktree, unmint_git_dest, wait_bounded,
};

/// Ceiling on one `on_mint` hook run.
pub(crate) const MINT_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// How much hook output a failure message quotes.
const MINT_HOOK_OUTPUT_BYTES: usize = 2048;

/// Runs a template's `on_mint` command once in the new copy:
/// a shell command with the supplied hook environment over a *cleared* environment,
/// cwd the copy, bounded by `MINT_HOOK_TIMEOUT`. Output is captured into a
/// scratch file so a chatty hook cannot block on a full pipe. Expected to run
/// with the tree mutex released. `Err` is the typed reason the spawn fails.
///
/// The only host variables that cross into the hook are `PATH` — so it can
/// find `cargo`/`npm` — and `HOME`, for their caches; they are read one name
/// at a time so the workspace gate can see exactly what crosses. Everything
/// else is dropped: `secrets.rs` reads `*_API_KEY` from the environment when
/// set, and a template-authored shell command has no business seeing the
/// user's keys. A hook that needs more declares it in its own `[env]`.
/// The tree adds `PLAK_LAUNCH_CHECKOUT`, `PLAK_PARENT_PATH`, and `PLAK_COPY_ID`
/// to the supplied map, overriding same-named template entries; the runner
/// forwards them without reading any additional host variables.
///
/// The command is handed to `sh -c` — this file's approved process constructor
/// in `tools/checks/check_workspace.py`; `git`'s constructor is in `git.rs` — so
/// it runs in `cwd` with no repository discovery in the way.
pub fn run_mint_hook(
    cwd: &Path,
    command: &str,
    env: &BTreeMap<String, String>,
) -> Result<(), String> {
    run_mint_hook_bounded(cwd, command, env, MINT_HOOK_TIMEOUT)
}

/// [`run_mint_hook`] with the ceiling spelled out, so a test need not wait
/// [`MINT_HOOK_TIMEOUT`] to watch the timeout work.
pub(super) fn run_mint_hook_bounded(
    cwd: &Path,
    command: &str,
    env: &BTreeMap<String, String>,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let scratch_dir =
        private_scratch().map_err(|error| format!("hook scratch was unavailable: {error}"))?;
    let scratch = scratch_dir.path().join("output");
    let log = create_private(&scratch)
        .map_err(|error| format!("hook output file could not be created: {error}"))?;
    let log_err = log
        .try_clone()
        .map_err(|error| format!("hook output file could not be shared: {error}"))?;
    let mut command_line = Command::new("sh");
    command_line
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err);
    if let Ok(value) = std::env::var("PATH") {
        command_line.env("PATH", value);
    }
    if let Ok(value) = std::env::var("HOME") {
        command_line.env("HOME", value);
    }
    command_line.envs(env);
    // The hook is a shell, and a shell's children outlive `kill(sh)`: a hook
    // that timed out left `cargo` running on the copy's target lock. Its own
    // process group makes the timeout able to reach everything it started.
    std::os::unix::process::CommandExt::process_group(&mut command_line, 0);
    let mut child = command_line
        .spawn()
        .map_err(|error| format!("hook could not start: {error}"))?;
    let status = match wait_bounded(&mut child, timeout) {
        Ok(Some(status)) => Ok(status),
        Ok(None) => Err(format!(
            "hook exceeded {}s and was killed",
            timeout.as_secs()
        )),
        Err(error) => Err(format!("hook could not be waited on: {error}")),
    };
    let output = std::fs::read(&scratch).unwrap_or_default();
    drop(scratch_dir);
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let tail_start = output.len().saturating_sub(MINT_HOOK_OUTPUT_BYTES);
    let tail = String::from_utf8_lossy(&output[tail_start..])
        .trim()
        .to_owned();
    Err(format!(
        "hook exited {}: {}",
        status
            .code()
            .map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
        if tail.is_empty() {
            "(no output)".to_owned()
        } else {
            tail
        }
    ))
}

pub(super) fn mint_git_worktree(
    launch: &Path,
    agent_id: &str,
    parent_cwd: &Path,
    mode: WorkspaceMode,
    over: Option<&VcsSnapshot>,
) -> Result<MintedCopy, VcsError> {
    let dest = launch.join(".plak/worktrees").join(agent_id);
    if dest.exists() || dest.is_symlink() {
        reclaim_orphan_worktree(launch, &dest, agent_id)?;
    }
    std::fs::create_dir_all(dest.parent().unwrap_or(launch))
        .map_err(|_| mint_error("could not create .plak/worktrees"))?;
    ensure_plak_mint_excludes(launch)?;
    // The base is the named candidate, or the parent's state at this instant.
    // Never the launch HEAD.
    let base = match over {
        Some(candidate) => candidate.clone(),
        None => git_snapshot(parent_cwd, agent_id)?,
    };
    let branch = (mode == WorkspaceMode::Fork).then(|| format!("plak/{agent_id}"));
    let dest_text = dest.display().to_string();
    let mut args = vec!["worktree", "add"];
    match branch.as_deref() {
        // `-B`: a `plak/<id>` branch left behind by an earlier tree in this
        // checkout (its worktree gone) is reset onto the new base instead of
        // refusing the mint. A branch still checked out elsewhere is refused
        // by git itself. The branch never holds work: seals pin candidates
        // under their own refs and never advance it.
        Some(branch) => {
            args.push("-B");
            args.push(branch);
        }
        // A snapshot copy takes no branch. It is thrown away unless
        // something seals and adopts it.
        None => args.push("--detach"),
    }
    args.push(dest_text.as_str());
    args.push(base.commit.as_str());
    let cleanup_branch = branch.clone().unwrap_or_default();
    let output =
        git_run(launch, &args).map_err(|_| VcsError::mint_failed("git worktree add failed"))?;
    if !output.status.success() {
        unmint_git_dest(launch, &dest, dest_text.as_str(), cleanup_branch.as_str());
        return Err(VcsError::mint_failed("git worktree add refused the mint"));
    }
    if let Err(error) = crate::git_redirect_fence::install(&dest) {
        unmint_git_dest(launch, &dest, dest_text.as_str(), cleanup_branch.as_str());
        return Err(mint_error(&format!(
            "git-redirect fence could not be installed: {error}"
        )));
    }
    Ok(MintedCopy {
        path: dest,
        branch,
        base,
    })
}

pub(super) fn mint_jj_workspace(
    launch: &Path,
    agent_id: &str,
    parent_cwd: &Path,
    over: Option<&VcsSnapshot>,
) -> Result<MintedCopy, VcsError> {
    let lock = jj_mint_lock(launch);
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::native_jj::mint(launch, agent_id, parent_cwd, over)
}
