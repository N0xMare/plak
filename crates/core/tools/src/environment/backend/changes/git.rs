use std::path::Path;

use plakous_model::CancellationToken;
use tokio::time::Instant;

use super::process::Executor;
use super::repository::{Repository, blocking, content, text};
use super::types::{Request, object, relative};
use super::{
    ChangesAnswer, ChangesContent, ChangesDiff, ChangesEntry, ChangesObservation, ChangesRefusal,
    ChangesRequest, ChangesStatus, MAX_CHANGES_ENTRIES, MAX_CHANGES_INPUT_BYTES,
    MAX_CHANGES_OUTPUT_BYTES,
};
use crate::environment::backend::ExecutionWorkspace;

pub(super) async fn execute(
    workspace: ExecutionWorkspace,
    request: ChangesRequest,
    deadline: Instant,
    cancellation: CancellationToken,
    machine: super::ChangesEnvironment,
) -> Result<ChangesAnswer, ChangesRefusal> {
    let executor = Executor::new(deadline, cancellation, machine.path.clone());
    supported(&executor, workspace.root()).await?;
    let known = match &request.0 {
        Request::Status { repository } => repository.clone(),
        Request::Diff { repository, .. } => Some(repository.toplevel.clone()),
    };
    let protected = blocking(move || {
        super::repository::protected_roots_from(
            machine.home.as_deref(),
            machine.product_home.as_deref(),
            machine.state_home.as_deref(),
        )
    })
    .await?;
    let repo = Repository::discover(workspace.clone(), known, executor, protected).await?;
    match request.0 {
        Request::Status { .. } => status(&workspace, &repo).await.map(ChangesAnswer::Status),
        Request::Diff {
            repository,
            observation,
            entry,
        } => {
            if repo.identity != repository {
                return Err(ChangesRefusal::Conflict);
            }
            diff(&workspace, &repo, observation, *entry)
                .await
                .map(ChangesAnswer::Diff)
        }
    }
}

pub(super) async fn supported(executor: &Executor, cwd: &Path) -> Result<(), ChangesRefusal> {
    let version = executor.run(cwd, None, None, &["--version"], 256).await?;
    if !version.success || !supported_version(&text(version.bytes)?) {
        return Err(ChangesRefusal::VcsUnavailable);
    }
    // A known fixture proves that the build actually honors attribute-source
    // selection. No object, index or fixture is written into the user's repo.
    let fixture = blocking(|| tempfile::tempdir().map_err(|_| ChangesRefusal::Io)).await?;
    let init = executor
        .run(
            fixture.path(),
            None,
            None,
            &["init", "--quiet", "--object-format=sha1"],
            4096,
        )
        .await?;
    if !init.success {
        return Err(ChangesRefusal::VcsUnavailable);
    }
    let path = fixture.path().join(".gitattributes");
    blocking(move || std::fs::write(path, "* changes-probe=set\n").map_err(|_| ChangesRefusal::Io))
        .await?;
    let empty = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let probe = executor
        .run(
            fixture.path(),
            None,
            Some(empty),
            &["check-attr", "-z", "changes-probe", "--", "probe"],
            1024,
        )
        .await?;
    if !probe.success || probe.bytes != b"probe\0changes-probe\0unspecified\0" {
        return Err(ChangesRefusal::VcsUnavailable);
    }
    Ok(())
}

pub(super) fn supported_version(version: &str) -> bool {
    let Some(version) = version.strip_prefix("git version ") else {
        return false;
    };
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    matches!((major, minor), (Some(major), Some(minor)) if (major, minor) >= (2, 43))
}

async fn observation(repo: &Repository) -> Result<ChangesObservation, ChangesRefusal> {
    let head = repo.query(&["rev-parse", "--verify", "HEAD"], 128).await?;
    let head = if head.success {
        let head = text(head.bytes)?;
        if !object(&head) {
            return Err(ChangesRefusal::UnsafeRepository);
        }
        Some(head)
    } else {
        // An unborn branch is different from an unreadable/corrupt HEAD.
        let symbolic = repo
            .query(&["symbolic-ref", "--quiet", "HEAD"], 4096)
            .await?;
        if !symbolic.success {
            return Err(ChangesRefusal::UnsafeRepository);
        }
        None
    };
    Ok(ChangesObservation {
        head,
        index: repo.index_identity().await?,
    })
}

async fn records(
    repo: &Repository,
    path: Option<&str>,
) -> Result<Vec<ChangesEntry>, ChangesRefusal> {
    let pathspec = repo.pathspec(path)?;
    let pathspec = if pathspec.is_empty() { "." } else { &pathspec };
    let result = repo
        .query(
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=normal",
                "--no-renames",
                "--ignore-submodules=all",
                "--",
                pathspec,
            ],
            MAX_CHANGES_OUTPUT_BYTES,
        )
        .await?;
    if !result.success {
        return Err(ChangesRefusal::VcsUnavailable);
    }
    parse_status(
        &result.bytes,
        &repo.identity.workspace,
        &repo.identity.toplevel,
    )
}

pub(super) fn parse_status(
    bytes: &[u8],
    workspace: &Path,
    toplevel: &Path,
) -> Result<Vec<ChangesEntry>, ChangesRefusal> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(ChangesRefusal::OutputLimit);
    }
    let mut entries = Vec::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        if entries.len() == MAX_CHANGES_ENTRIES {
            return Err(ChangesRefusal::OutputLimit);
        }
        let record = std::str::from_utf8(record).map_err(|_| ChangesRefusal::UnsafeRepository)?;
        let (state, head, index, path) = if let Some(path) = record.strip_prefix("? ") {
            ("??", None, None, path)
        } else if record.starts_with("1 ") {
            let fields = record.splitn(9, ' ').collect::<Vec<_>>();
            if fields.len() != 9 || !object(fields[6]) || !object(fields[7]) {
                return Err(ChangesRefusal::UnsafeRepository);
            }
            (fields[1], nonzero(fields[6]), nonzero(fields[7]), fields[8])
        } else if record.starts_with("u ") {
            let fields = record.splitn(11, ' ').collect::<Vec<_>>();
            if fields.len() != 11 {
                return Err(ChangesRefusal::UnsafeRepository);
            }
            (fields[1], None, None, fields[10])
        } else {
            return Err(ChangesRefusal::UnsafeRepository);
        };
        relative(path)?;
        let path = toplevel
            .join(path)
            .strip_prefix(workspace)
            .map_err(|_| ChangesRefusal::UnsafeRepository)?
            .to_path_buf();
        let path = if path.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            path.to_str()
                .ok_or(ChangesRefusal::UnsafeRepository)?
                .to_owned()
        };
        relative(&path)?;
        if state.len() != 2 {
            return Err(ChangesRefusal::UnsafeRepository);
        }
        entries.push(ChangesEntry {
            path,
            state: state.to_owned(),
            head_object: head,
            index_object: index,
            content: ChangesContent::Missing,
        });
    }
    Ok(entries)
}

fn nonzero(value: &str) -> Option<String> {
    value
        .bytes()
        .any(|byte| byte != b'0')
        .then(|| value.to_owned())
}

async fn status(
    workspace: &ExecutionWorkspace,
    repo: &Repository,
) -> Result<ChangesStatus, ChangesRefusal> {
    let observed = observation(repo).await?;
    let mut entries = records(repo, None).await?;
    for entry in &mut entries {
        repo.executor.check()?;
        entry.content = content(workspace, &entry.path).await?;
    }
    // Status is not atomic: no returned identity may be assembled from inputs
    // that changed during this read. Refuse the entire observation on drift.
    if observation(repo).await? != observed {
        return Err(ChangesRefusal::Conflict);
    }
    let again = records(repo, None).await?;
    if entries.len() != again.len()
        || entries.iter().zip(&again).any(|(a, b)| {
            a.path != b.path
                || a.state != b.state
                || a.head_object != b.head_object
                || a.index_object != b.index_object
        })
    {
        return Err(ChangesRefusal::Conflict);
    }
    for entry in &entries {
        if content(workspace, &entry.path).await? != entry.content {
            return Err(ChangesRefusal::Conflict);
        }
    }
    let branch = repo
        .query(&["symbolic-ref", "--quiet", "--short", "HEAD"], 4096)
        .await?;
    let label = if branch.success {
        text(branch.bytes)?
    } else {
        observed.head.clone().unwrap_or_else(|| "unborn".into())
    };
    let status = ChangesStatus {
        repository: repo.identity.clone(),
        observation: observed,
        base_label: format!("work tree vs HEAD · {label}"),
        entries,
    };
    // The answer, not only the porcelain pipe, is bounded.
    if serde_json::to_vec(&status)
        .map_err(|_| ChangesRefusal::Io)?
        .len()
        > MAX_CHANGES_OUTPUT_BYTES
    {
        return Err(ChangesRefusal::OutputLimit);
    }
    Ok(status)
}

async fn matches_entry(
    workspace: &ExecutionWorkspace,
    repo: &Repository,
    observed: &ChangesObservation,
    entry: &ChangesEntry,
) -> Result<(), ChangesRefusal> {
    if observation(repo).await? != *observed {
        return Err(ChangesRefusal::Conflict);
    }
    let mut current = records(repo, Some(&entry.path)).await?;
    if current.len() != 1 {
        return Err(ChangesRefusal::Conflict);
    }
    current[0].content = content(workspace, &entry.path).await?;
    if current[0] != *entry {
        return Err(ChangesRefusal::Conflict);
    }
    Ok(())
}

async fn diff(
    workspace: &ExecutionWorkspace,
    repo: &Repository,
    observed: ChangesObservation,
    entry: ChangesEntry,
) -> Result<ChangesDiff, ChangesRefusal> {
    matches_entry(workspace, repo, &observed, &entry).await?;
    if entry.state == "??" || matches!(entry.content, ChangesContent::Unsupported) {
        return Err(ChangesRefusal::VcsUnsupported);
    }
    if matches!(entry.content, ChangesContent::TooLarge { .. }) {
        return Err(ChangesRefusal::TooLarge);
    }
    let path = repo.pathspec(Some(&entry.path))?;
    let objects = if entry.state.contains('U') || matches!(entry.state.as_str(), "AA" | "DD") {
        unmerged_objects(repo, observed.head.as_deref(), &path).await?
    } else {
        [entry.head_object.clone(), entry.index_object.clone()]
            .into_iter()
            .flatten()
            .collect()
    };
    for oid in &objects {
        let size = repo.query(&["cat-file", "-s", oid], 128).await?;
        if !size.success {
            return Err(ChangesRefusal::VcsUnavailable);
        }
        if text(size.bytes)?
            .parse::<u64>()
            .map_err(|_| ChangesRefusal::UnsafeRepository)?
            > MAX_CHANGES_INPUT_BYTES
        {
            return Err(ChangesRefusal::TooLarge);
        }
    }
    matches_entry(workspace, repo, &observed, &entry).await?;
    let base = observed.head.as_deref().unwrap_or(&repo.attributes);
    let fixed = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=all",
        "--no-renames",
    ];
    let mut args = fixed.to_vec();
    args.extend(["--numstat", "-z", base, "--", &path]);
    let numstat = repo.query(&args, MAX_CHANGES_OUTPUT_BYTES).await?;
    if !numstat.success {
        return Err(ChangesRefusal::VcsUnavailable);
    }
    let binary = validate_numstat(&numstat.bytes, &path)?;
    let numstat = String::from_utf8(numstat.bytes).map_err(|_| ChangesRefusal::UnsafeRepository)?;
    let result = if binary {
        ChangesDiff::Binary { numstat }
    } else {
        let mut args = fixed.to_vec();
        args.extend([
            "--patch",
            "--no-color",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
            "--",
            &path,
        ]);
        let output = repo.query(&args, MAX_CHANGES_OUTPUT_BYTES).await?;
        if !output.success {
            return Err(ChangesRefusal::VcsUnavailable);
        }
        match String::from_utf8(output.bytes) {
            Ok(text) => ChangesDiff::Patch { text },
            Err(_) => ChangesDiff::Binary { numstat },
        }
    };
    matches_entry(workspace, repo, &observed, &entry).await?;
    Ok(result)
}

async fn unmerged_objects(
    repo: &Repository,
    head: Option<&str>,
    path: &str,
) -> Result<Vec<String>, ChangesRefusal> {
    let mut queries = Vec::new();
    if let Some(head) = head {
        queries.push((vec!["ls-tree", "-z", head, "--", path], 2, 1));
    }
    queries.push((vec!["ls-files", "--stage", "-z", "--", path], 1, 3));
    let mut objects = Vec::new();
    for (args, oid_column, maximum) in queries {
        let result = repo.query(&args, 16 * 1024).await?;
        if !result.success {
            return Err(ChangesRefusal::VcsUnavailable);
        }
        if !result.bytes.is_empty() && !result.bytes.ends_with(&[0]) {
            return Err(ChangesRefusal::OutputLimit);
        }
        let records = result
            .bytes
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .collect::<Vec<_>>();
        if records.len() > maximum {
            return Err(ChangesRefusal::OutputLimit);
        }
        for record in records {
            let record =
                std::str::from_utf8(record).map_err(|_| ChangesRefusal::UnsafeRepository)?;
            let (metadata, found_path) = record
                .split_once('\t')
                .ok_or(ChangesRefusal::UnsafeRepository)?;
            let fields = metadata.split_ascii_whitespace().collect::<Vec<_>>();
            if found_path != path || fields.len() != 3 || !object(fields[oid_column]) {
                return Err(ChangesRefusal::UnsafeRepository);
            }
            if oid_column == 1 && !matches!(fields[2], "1" | "2" | "3") {
                return Err(ChangesRefusal::Conflict);
            }
            objects.push(fields[oid_column].to_owned());
        }
    }
    objects.sort();
    objects.dedup();
    Ok(objects)
}

fn validate_numstat(bytes: &[u8], expected: &str) -> Result<bool, ChangesRefusal> {
    if bytes.is_empty() {
        return Ok(false);
    }
    if !bytes.ends_with(&[0]) {
        return Err(ChangesRefusal::OutputLimit);
    }
    let fields = std::str::from_utf8(&bytes[..bytes.len() - 1])
        .map_err(|_| ChangesRefusal::UnsafeRepository)?
        .splitn(3, '\t')
        .collect::<Vec<_>>();
    if fields.len() != 3
        || fields[2] != expected
        || !fields[..2]
            .iter()
            .all(|value| *value == "-" || value.parse::<u64>().is_ok())
    {
        return Err(ChangesRefusal::UnsafeRepository);
    }
    Ok(fields[0] == "-" && fields[1] == "-")
}
