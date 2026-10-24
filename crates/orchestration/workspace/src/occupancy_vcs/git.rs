//! Git CLI snapshots, adopts, excludes, and worktree registration.

use std::process::{Command, Output};

use super::{
    AdoptFailure, AdoptLanding, ChangeKind, ChangedPath, GIT_CLI_ENABLED, Path, VcsError,
    VcsErrorKind, VcsSnapshot, create_private, mint_error, private_scratch, wait_bounded,
};

/// Ceiling on a `git` read: status, rev-parse, counts, name lists.
pub(super) const GIT_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Ceiling on a `git` step that mints, seals, adopts, or unmints a copy.
const GIT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Runs `command` to completion or to `timeout`, whichever comes first. A
/// `git` that stalls (a lock, a credential prompt, a hung filesystem) fails
/// with `TimedOut` instead of holding the seal, adopt, or mint forever.
///
/// Output goes to scratch files, as the mint hook's does: nothing the command
/// leaves running can hold a pipe this process must drain.
pub(super) fn output_within(
    mut command: Command,
    timeout: std::time::Duration,
) -> std::io::Result<Output> {
    let scratch = private_scratch()?;
    let stdout_path = scratch.path().join("stdout");
    let stderr_path = scratch.path().join("stderr");
    let captured = (|| {
        command
            .stdin(std::process::Stdio::null())
            .stdout(create_private(&stdout_path)?)
            .stderr(create_private(&stderr_path)?);
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn()?;
        match wait_bounded(&mut child, timeout)? {
            Some(status) => Ok(Output {
                status,
                stdout: std::fs::read(&stdout_path)?,
                stderr: std::fs::read(&stderr_path)?,
            }),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("git exceeded {}s and was killed", timeout.as_secs()),
            )),
        }
    })();
    drop(scratch);
    captured
}

/// Whether `git worktree list` still names `path`. Git records the real
/// path, so a missing directory is also compared through the real path of its
/// deepest existing ancestor.
pub(super) fn git_worktree_registered(launch: &Path, path: &Path) -> Result<bool, VcsError> {
    let listed = git_run(launch, &["worktree", "list", "--porcelain", "-z"])
        .ok()
        .filter(|output| output.status.success())
        .ok_or_else(|| {
            VcsError::new(
                VcsErrorKind::RecoveryRequired,
                "Git worktree registrations could not be listed",
            )
        })?;
    let mut spellings = vec![path.to_path_buf()];
    if let Some(real) = path.ancestors().find_map(|ancestor| {
        let real = ancestor.canonicalize().ok()?;
        Some(real.join(path.strip_prefix(ancestor).ok()?))
    }) {
        spellings.push(real);
    }
    Ok(listed.stdout.split(|byte| *byte == 0).any(|field| {
        spellings
            .iter()
            .any(|spelling| field == format!("worktree {}", spelling.display()).as_bytes())
    }))
}

/// The one `git` constructor in this crate; every call builds on it.
///
/// Repository-controlled code never runs inside Plak's own git steps: the
/// inherited `GIT_*` steering variables are dropped (a caller re-adds only the
/// ones it sets deliberately), hooks resolve under `/dev/null` and so never exist,
/// and the fsmonitor daemon hook is off. `git` never prompts: no terminal
/// prompt and no askpass helper, so a remote or credential question fails
/// instead of waiting for input nobody can give.
pub(super) fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    strip_git_environment(&mut command);
    command
        .current_dir(cwd)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "");
    // Without this, an unconfigured identity makes git resolve the host's
    // FQDN for every reflog entry; musl's resolver waits out its full timeout
    // when there is no network. Configured `user.email` still takes precedence.
    if std::env::var_os("EMAIL").is_none() {
        command.env("EMAIL", FALLBACK_EMAIL);
    }
    command
}

/// Identity fallback for reflog entries Plak's git steps write when neither
/// git configuration nor the environment names one.
pub(super) const FALLBACK_EMAIL: &str = "plak@local";

/// Every variable git reads to relocate the repository, inject configuration,
/// run an external program, or change identity, refs, attributes or tracing.
/// A fixed list because the CLI boundary forbids ambient environment
/// iteration; removing `GIT_CONFIG_COUNT` leaves any `GIT_CONFIG_KEY_<n>` inert.
const STEERING_GIT_VARIABLES: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_ASKPASS",
    "GIT_ATTR_NOSYSTEM",
    "GIT_ATTR_SOURCE",
    "GIT_AUTHOR_DATE",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_NAME",
    "GIT_CEILING_DIRECTORIES",
    "GIT_COMMITTER_DATE",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_DEFAULT_HASH",
    "GIT_DEFAULT_REF_FORMAT",
    "GIT_DIFF_OPTS",
    "GIT_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_EDITOR",
    "GIT_EXEC_PATH",
    "GIT_EXTERNAL_DIFF",
    "GIT_GLOB_PATHSPECS",
    "GIT_GRAFT_FILE",
    "GIT_ICASE_PATHSPECS",
    "GIT_INDEX_FILE",
    "GIT_INDEX_VERSION",
    "GIT_LITERAL_PATHSPECS",
    "GIT_NAMESPACE",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_OPTIONAL_LOCKS",
    "GIT_PAGER",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_REPLACE_REF_BASE",
    "GIT_SEQUENCE_EDITOR",
    "GIT_SHALLOW_FILE",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_TEMPLATE_DIR",
    "GIT_TERMINAL_PROMPT",
    "GIT_TRACE",
    "GIT_TRACE2",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
    "GIT_TRACE_PACKET",
    "GIT_TRACE_PERFORMANCE",
    "GIT_TRACE_SETUP",
    "GIT_WORK_TREE",
];

pub(super) fn strip_git_environment(command: &mut Command) {
    for name in STEERING_GIT_VARIABLES {
        command.env_remove(name);
    }
}

/// A `git` step that changes a copy, bounded by [`GIT_WRITE_TIMEOUT`].
pub(super) fn git_run(cwd: &Path, args: &[&str]) -> Result<Output, std::io::Error> {
    if !GIT_CLI_ENABLED {
        return Err(std::io::Error::other("Git CLI backend not included"));
    }
    let mut command = git_command(cwd);
    command.args(args);
    output_within(command, GIT_WRITE_TIMEOUT)
}

/// A `git` read, bounded by [`GIT_QUERY_TIMEOUT`]. It takes no optional locks,
/// so a status poll never contends with the agent's own `git`.
pub(crate) fn git_query(cwd: &Path, args: &[&str]) -> Result<Output, std::io::Error> {
    if !GIT_CLI_ENABLED {
        return Err(std::io::Error::other("Git CLI backend not included"));
    }
    let mut command = git_command(cwd);
    command.args(args).env("GIT_OPTIONAL_LOCKS", "0");
    output_within(command, GIT_QUERY_TIMEOUT)
}

/// `git` with the snapshot's temp index bound. Every index-writing step of
/// `snapshot` runs through here so the real index is never touched.
fn git_run_indexed(cwd: &Path, index: &Path, args: &[&str]) -> Result<Output, std::io::Error> {
    let mut command = git_command(cwd);
    command.args(args).env("GIT_INDEX_FILE", index);
    output_within(command, GIT_WRITE_TIMEOUT)
}

pub(super) fn git_text(cwd: &Path, args: &[&str], what: &str) -> Result<String, VcsError> {
    let output = git_query(cwd, args).map_err(|_| mint_error(what))?;
    if !output.status.success() {
        return Err(mint_error(what));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Fixed author/committer date for snapshot wrapper commits.
const SNAPSHOT_DATE: &str = "@0 +0000";

pub(super) fn git_branch(cwd: &Path) -> Option<String> {
    let output = git_query(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|branch| !branch.is_empty())
}

pub(super) fn git_snapshot(path: &Path, label: &str) -> Result<VcsSnapshot, VcsError> {
    let reported = git_text(
        path,
        &["rev-parse", "--git-path", "index"],
        "git index path could not be resolved",
    )?;
    if reported.is_empty() {
        return Err(mint_error("git index path was empty"));
    }
    let index_source = {
        let reported = Path::new(&reported);
        if reported.is_absolute() {
            reported.to_path_buf()
        } else {
            path.join(reported)
        }
    };
    let scratch = private_scratch().map_err(|_| mint_error("snapshot scratch was unavailable"))?;
    let temp_index = scratch.path().join("index");
    // A missing source index is an empty index; a zero-byte file is not a
    // valid index, so leave the temp path absent rather than creating one.
    if index_source.exists() {
        let seeded = std::fs::File::open(&index_source)
            .and_then(|mut source| std::io::copy(&mut source, &mut create_private(&temp_index)?));
        if seeded.is_err() {
            return Err(mint_error("snapshot index could not be seeded"));
        }
    }
    git_snapshot_indexed(path, &temp_index, label)
}

fn git_snapshot_indexed(
    path: &Path,
    temp_index: &Path,
    label: &str,
) -> Result<VcsSnapshot, VcsError> {
    let staged = git_run_indexed(path, temp_index, &["add", "-A"])
        .map_err(|_| mint_error("git add could not stage the snapshot"))?;
    if !staged.status.success() {
        return Err(mint_error("git add refused the snapshot"));
    }
    unstage_secret_paths(path, temp_index)?;
    let output = git_run_indexed(path, temp_index, &["write-tree"])
        .map_err(|_| mint_error("git write-tree failed"))?;
    if !output.status.success() {
        return Err(mint_error("git write-tree refused the snapshot"));
    }
    let content = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if content.is_empty() {
        return Err(mint_error("git write-tree produced no tree"));
    }
    let message = format!("plak snapshot {label}");
    let head = git_run(path, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|head| !head.is_empty());
    let mut args = vec![
        "-c",
        "user.name=plak",
        "-c",
        "user.email=plak@local",
        // A fixed date keeps the wrapper commit a pure function of the tree,
        // HEAD, and the label: no wall clock in a candidate's identity.
        "-c",
        "commit.gpgsign=false",
        "commit-tree",
        content.as_str(),
    ];
    if let Some(head) = head.as_deref() {
        args.push("-p");
        args.push(head);
    }
    args.push("-m");
    args.push(message.as_str());
    let mut command = git_command(path);
    command
        .args(&args)
        .env("GIT_AUTHOR_DATE", SNAPSHOT_DATE)
        .env("GIT_COMMITTER_DATE", SNAPSHOT_DATE);
    let output = output_within(command, GIT_WRITE_TIMEOUT)
        .map_err(|_| mint_error("git commit-tree failed"))?;
    if !output.status.success() {
        return Err(mint_error("git commit-tree refused the snapshot"));
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if commit.is_empty() {
        return Err(mint_error("git commit-tree produced no commit"));
    }
    Ok(VcsSnapshot {
        history_left_base: false,
        commit,
        content,
        change_id: None,
    })
}

pub(super) fn git_adopt(
    target_path: &Path,
    base_commit: &str,
    candidate_commit: &str,
    expected_target: Option<&str>,
) -> Result<AdoptLanding, AdoptFailure> {
    let target = git_snapshot(target_path, "adopt-target")
        .map_err(|error| AdoptFailure::Unavailable(error.detail))?;
    if let Some(expected) = expected_target
        && !target.matches_expectation(expected)
    {
        return Err(AdoptFailure::TargetChanged(target.token()));
    }
    let merged = git_run(
        target_path,
        &[
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={base_commit}"),
            target.commit.as_str(),
            candidate_commit,
        ],
    )
    .map_err(|_| AdoptFailure::Unavailable("git merge-tree failed".to_owned()))?;
    let stdout = String::from_utf8_lossy(&merged.stdout).into_owned();
    if !merged.status.success() {
        // `merge-tree --write-tree` exits 1 for a content conflict and >1 for
        // anything else (bad base object, corrupt ref). Only the former is a
        // `merge_conflict`; the rest is the VCS refusing to run the merge.
        if merged.status.code() == Some(1) {
            return Err(AdoptFailure::Conflict(parse_merge_tree_conflicts(&stdout)));
        }
        return Err(AdoptFailure::Unavailable(format!(
            "git merge-tree exited {}: {}",
            merged
                .status
                .code()
                .map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
            stderr_excerpt(&merged.stderr)
        )));
    }
    let Some(merged_tree) = stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Err(AdoptFailure::Unavailable(
            "git merge-tree produced no tree".to_owned(),
        ));
    };
    let names = git_run(
        target_path,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-status",
            "-z",
            "-M",
            target.content.as_str(),
            merged_tree,
        ],
    )
    .map_err(|_| AdoptFailure::Unavailable("git diff failed".to_owned()))?;
    if !names.status.success() {
        return Err(AdoptFailure::Unavailable(
            "git diff refused the merged tree".to_owned(),
        ));
    }
    let files = parse_name_status(&names.stdout)?;
    let files_changed = u32::try_from(files.len()).unwrap_or(u32::MAX);
    if files_changed == 0 {
        return Ok(AdoptLanding {
            target_before: target.commit.clone(),
            target_after: target.commit,
            files_changed: 0,
            files,
            child_base: None,
        });
    }
    let patch = git_run(
        target_path,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "-M",
            target.content.as_str(),
            merged_tree,
        ],
    )
    .map_err(|_| AdoptFailure::Unavailable("git diff failed".to_owned()))?;
    if !patch.status.success() {
        return Err(AdoptFailure::Unavailable(
            "git diff refused the merged tree".to_owned(),
        ));
    }
    // `git apply` is atomic by default, worktree-only, binary-safe, and never
    // commits — the human checkout keeps its own HEAD and its own index.
    if let Err(stderr) = git_apply(target_path, &patch.stdout) {
        // The patch was cut against the target snapshot taken above; a hunk
        // that no longer applies means the worktree moved in between. Any
        // other refusal (mode, CRLF, permissions) is the VCS, not a race.
        let moved = [
            "does not match index",
            "patch does not apply",
            "already exists",
        ]
        .iter()
        .any(|needle| stderr.contains(needle));
        if moved {
            return Err(AdoptFailure::TargetChanged(target.token()));
        }
        return Err(AdoptFailure::Unavailable(format!(
            "git apply refused the merged patch: {stderr}"
        )));
    }
    let after = git_snapshot(target_path, "adopt-after")
        .map_err(|error| AdoptFailure::Unavailable(error.detail))?;
    Ok(AdoptLanding {
        target_before: target.commit,
        target_after: after.commit,
        files_changed,
        files,
        child_base: None,
    })
}

/// `git diff --name-status -z -M`: one record is `<status> NUL <path> NUL`,
/// and a rename is `<Rscore> NUL <old> NUL <new> NUL`.
fn parse_name_status(stdout: &[u8]) -> Result<Vec<ChangedPath>, AdoptFailure> {
    let mut parts = stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    if parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    let mut files = Vec::new();
    let mut index = 0;
    while index < parts.len() {
        let status = std::str::from_utf8(parts[index]).map_err(|_| {
            AdoptFailure::Unavailable("git diff name-status was not utf-8".to_owned())
        })?;
        let kind = status.as_bytes().first().copied().ok_or_else(|| {
            AdoptFailure::Unavailable("git diff name-status was empty".to_owned())
        })?;
        // A rename carries the old path. A copy (`-C`, or a repo that detects
        // copies) is an added destination; the source is unchanged, and the
        // receipt vocabulary has no copy letter.
        if matches!(kind, b'R' | b'C') {
            if index + 2 >= parts.len() {
                return Err(AdoptFailure::Unavailable(
                    "git diff name-status rename was truncated".to_owned(),
                ));
            }
            let from = utf8_path(parts[index + 1]);
            let path = utf8_path(parts[index + 2]);
            if kind == b'C' {
                files.push(ChangedPath {
                    status: ChangeKind::Added,
                    path,
                    from: None,
                });
            } else {
                files.push(ChangedPath {
                    status: ChangeKind::Renamed,
                    path,
                    from: Some(from),
                });
            }
            index += 3;
            continue;
        }
        let parsed = match kind {
            b'A' => ChangeKind::Added,
            b'M' => ChangeKind::Modified,
            b'D' => ChangeKind::Deleted,
            b'T' => ChangeKind::TypeChanged,
            _ => {
                return Err(AdoptFailure::Unavailable(format!(
                    "git diff name-status {status} is not a landing status"
                )));
            }
        };
        if index + 1 >= parts.len() {
            return Err(AdoptFailure::Unavailable(
                "git diff name-status record was truncated".to_owned(),
            ));
        }
        files.push(ChangedPath {
            status: parsed,
            path: utf8_path(parts[index + 1]),
            from: None,
        });
        index += 2;
    }
    Ok(files)
}

/// A non-UTF-8 path is lossy so the landing still records the file. Git's
/// `-z` record is bytes; refusing the adopt for one path would drop the rest.
fn utf8_path(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Applies the merged-tree patch to the target's working copy. The patch goes
/// through a scratch file rather than a pipe so this module keeps one process
/// constructor per VCS. `Err` carries git's stderr excerpt.
fn git_apply(target_path: &Path, patch: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let Ok(scratch) = private_scratch() else {
        return Err("the patch scratch directory could not be created".to_owned());
    };
    let patch_path = scratch.path().join("adopt.patch");
    if create_private(&patch_path)
        .and_then(|mut file| file.write_all(patch))
        .is_err()
    {
        return Err("the patch scratch file could not be written".to_owned());
    }
    let patch_text = patch_path.display().to_string();
    // `git apply` is atomic by default, worktree-only, and binary-safe.
    let applied = git_run(target_path, &["apply", patch_text.as_str()]);
    drop(scratch);
    match applied {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(stderr_excerpt(&output.stderr)),
        Err(error) => Err(format!("git apply could not start: {error}")),
    }
}

/// How much subprocess stderr a typed failure quotes.
pub(super) const STDERR_EXCERPT_BYTES: usize = 240;

/// The first `STDERR_EXCERPT_BYTES` of a subprocess's stderr, one line.
pub(super) fn stderr_excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let joined: String = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if joined.is_empty() {
        return "(no stderr)".to_owned();
    }
    let mut end = joined.len().min(STDERR_EXCERPT_BYTES);
    while end > 0 && !joined.is_char_boundary(end) {
        end -= 1;
    }
    joined[..end].to_owned()
}

/// `git merge-tree --write-tree` prints the tree, then a conflicted-file info
/// section of `<mode> <object> <stage>\t<path>` rows, then the messages.
pub(super) fn parse_merge_tree_conflicts(stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in stdout.lines().skip(1) {
        if line.trim().is_empty() {
            break;
        }
        let Some((_, path)) = line.rsplit_once('\t') else {
            continue;
        };
        let path = path.trim().to_owned();
        if !path.is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

pub(super) fn git_worktree_dirty(path: &Path) -> bool {
    git_query(path, &["status", "--porcelain", "-uall"])
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

pub(super) fn git_ahead_behind(child: &Path, parent: &Path) -> Option<(u64, u64)> {
    let parent_head = git_query(parent, &["rev-parse", "HEAD"]).ok()?;
    if !parent_head.status.success() {
        return None;
    }
    let parent_sha = String::from_utf8_lossy(&parent_head.stdout)
        .trim()
        .to_owned();
    if parent_sha.is_empty() {
        return None;
    }
    let counted = git_query(
        child,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{parent_sha}"),
        ],
    )
    .ok()?;
    if !counted.status.success() {
        return None;
    }
    parse_left_right_counts(&String::from_utf8_lossy(&counted.stdout))
}

pub(super) fn parse_left_right_counts(text: &str) -> Option<(u64, u64)> {
    let mut parts = text.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

const SECRET_NAMES: &[&str] = &[
    ".netrc",
    ".npmrc",
    "credentials.json",
    "id_rsa",
    "id_ed25519",
];

const SECRET_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx"];

pub(crate) fn secret_fileset() -> String {
    let mut patterns = vec![
        "root-glob-i:'**/.env'".to_owned(),
        "(root-glob-i:'**/.env.*' ~ root-glob-i:'**/.env.example')".to_owned(),
    ];
    patterns.extend(
        SECRET_NAMES
            .iter()
            .map(|name| format!("root-glob-i:'**/{name}'")),
    );
    patterns.extend(
        SECRET_EXTENSIONS
            .iter()
            .map(|ext| format!("root-glob-i:'**/*.{ext}'")),
    );
    patterns.join(" | ")
}

pub(crate) fn is_secret_git_path(path: &str) -> bool {
    let file = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    let lower = file.to_ascii_lowercase();
    lower == ".env"
        || (lower.starts_with(".env.") && lower != ".env.example")
        || SECRET_NAMES.contains(&lower.as_str())
        || lower
            .rsplit_once('.')
            .is_some_and(|(_, ext)| SECRET_EXTENSIONS.contains(&ext))
}

/// Keeps a snapshot commit from pinning a secret the tool policy already
/// excludes. Runs against the snapshot's temp index, never the
/// real one.
fn unstage_secret_paths(child: &Path, index: &Path) -> Result<(), VcsError> {
    let listed = git_run_indexed(child, index, &["diff", "--cached", "--name-only"])
        .map_err(|_| mint_error("git diff --cached failed on the snapshot index"))?;
    let listed = if listed.status.success() {
        String::from_utf8_lossy(&listed.stdout).into_owned()
    } else {
        // No HEAD to diff against: every staged path is new.
        let all = git_run_indexed(child, index, &["ls-files", "--cached"])
            .map_err(|_| mint_error("git ls-files failed on the snapshot index"))?;
        if !all.status.success() {
            return Err(mint_error("snapshot index could not be listed"));
        }
        String::from_utf8_lossy(&all.stdout).into_owned()
    };
    for path in listed.lines() {
        let path = path.trim();
        if path.is_empty() || !is_secret_git_path(path) {
            continue;
        }
        let reset = git_run_indexed(child, index, &["reset", "-q", "--", path])
            .map_err(|_| mint_error("git reset failed on the snapshot index"))?;
        if reset.status.success() {
            continue;
        }
        // Unborn HEAD: there is nothing to reset the entry back to.
        let removed = git_run_indexed(
            child,
            index,
            &["rm", "--cached", "--force", "-q", "--", path],
        )
        .map_err(|_| mint_error("git rm --cached failed on the snapshot index"))?;
        if !removed.status.success() {
            return Err(mint_error("a secret path could not be unstaged"));
        }
    }
    Ok(())
}

pub(super) fn ensure_plak_mint_excludes(launch: &Path) -> Result<(), VcsError> {
    let git = launch.join(".git");
    if !git.exists() {
        return Ok(());
    }
    let reported = git_text(
        launch,
        &["rev-parse", "--git-path", "info/exclude"],
        "git exclude path could not be resolved",
    )?;
    if reported.is_empty() {
        return Err(mint_error("git exclude path was empty"));
    }
    let path = {
        let reported = Path::new(&reported);
        if reported.is_absolute() {
            reported.to_path_buf()
        } else {
            launch.join(reported)
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| mint_error("git exclude directory could not be created"))?;
    }
    super::append_exclude_lines(&path)?;
    Ok(())
}

/// Removes a copy a crash left between `git worktree add` and the tree
/// recording it. Copy ids are tree-scoped and the tree never mints an id it has
/// recorded, so an existing destination for this id was never handed to an
/// agent: its registration, directory and `plak/<copy_id>` branch all go.
pub(super) fn reclaim_orphan_worktree(
    launch: &Path,
    dest: &Path,
    copy_id: &str,
) -> Result<(), VcsError> {
    let dest_text = dest.display().to_string();
    let _ = git_run(
        launch,
        &["worktree", "remove", "--force", dest_text.as_str()],
    );
    if dest.is_symlink() {
        std::fs::remove_file(dest)
    } else if dest.exists() {
        std::fs::remove_dir_all(dest)
    } else {
        Ok(())
    }
    .map_err(|_| VcsError::mint_failed("an orphaned worktree directory could not be removed"))?;
    let pruned = git_run(launch, &["worktree", "prune"])
        .map_err(|_| VcsError::mint_failed("git worktree prune could not run"))?;
    if !pruned.status.success() {
        return Err(VcsError::mint_failed("git worktree prune refused"));
    }
    let branch = format!("plak/{copy_id}");
    let reference = format!("refs/heads/{branch}");
    let present = git_run(
        launch,
        &["show-ref", "--verify", "--quiet", reference.as_str()],
    )
    .map_err(|_| VcsError::mint_failed("orphan branch state could not be checked"))?;
    if present.status.success() {
        let removed = git_run(launch, &["branch", "-D", branch.as_str()])
            .map_err(|_| VcsError::mint_failed("orphan branch cleanup could not run"))?;
        if !removed.status.success() {
            return Err(VcsError::mint_failed("orphan branch cleanup failed"));
        }
    }
    Ok(())
}

pub(super) fn unmint_git_dest(launch: &Path, dest: &Path, dest_text: &str, branch: &str) {
    let _ = git_run(launch, &["worktree", "remove", "--force", dest_text]);
    let _ = std::fs::remove_dir_all(dest);
    if !branch.is_empty() {
        // Safe even with an unadopted candidate: the candidate's commit is
        // pinned under `refs/plak/<namespace>/candidates/<id>`, which is never removed here.
        let _ = git_run(launch, &["branch", "-D", branch]);
    }
}
