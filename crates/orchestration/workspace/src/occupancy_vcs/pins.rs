//! Atomic Git provenance publication and removal.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::{VcsError, git_query, namespace};

pub(super) fn adoption(
    request: &super::AdoptRequest<'_>,
    before: &str,
    after: &str,
) -> Vec<(String, String)> {
    let id = request.candidate_id;
    let mut refs = vec![
        (
            reference(request.namespace, "adopted", &format!("{id}-before")),
            before.to_owned(),
        ),
        (
            reference(request.namespace, "adopted", &format!("{id}-after")),
            after.to_owned(),
        ),
    ];
    if let Some(agent) = request.source_agent_id {
        refs.push((
            reference(request.namespace, "bases", agent),
            request
                .resolved_source
                .as_ref()
                .filter(|source| source.agent_id == Some(agent))
                .map_or_else(
                    || request.candidate.commit.clone(),
                    |source| source.commit.to_owned(),
                ),
        ));
    }
    if let Some(source) = &request.resolved_source {
        for (suffix, commit) in [("before", before), ("after", after)] {
            refs.push((
                reference(
                    request.namespace,
                    "adopted",
                    &format!("{}-{suffix}", source.candidate_id),
                ),
                commit.to_owned(),
            ));
        }
        if let Some(agent) = source
            .agent_id
            .filter(|agent| Some(*agent) != request.source_agent_id)
        {
            refs.push((
                reference(request.namespace, "bases", agent),
                source.commit.to_owned(),
            ));
        }
    }
    refs
}

pub(super) fn reference(namespace: &str, family: &str, id: &str) -> String {
    format!(
        "refs/plak/{}/{family}/{id}",
        namespace.get(..16).unwrap_or(namespace)
    )
}

pub(super) fn common(path: &Path) -> Result<PathBuf, VcsError> {
    let output = git_query(path, &["rev-parse", "--git-common-dir"])
        .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    if !output.status.success() {
        return Err(VcsError::seal_failed("cannot locate provenance repository"));
    }
    Ok(path.join(String::from_utf8_lossy(&output.stdout).trim()))
}

pub(super) fn update(
    path: &Path,
    namespace: &str,
    refs: &[(String, Option<&str>)],
    create_only: bool,
) -> Result<(), VcsError> {
    let _guard = namespace::reserve(&common(path)?, namespace)?;
    update_reserved(path, refs, create_only)
}

/// Caller holds the namespace reservation through publication.
pub(super) fn update_reserved(
    path: &Path,
    refs: &[(String, Option<&str>)],
    create_only: bool,
) -> Result<(), VcsError> {
    let mut transaction = String::from("start\n");
    for (name, commit) in refs {
        if name.bytes().any(|byte| byte.is_ascii_whitespace())
            || commit.is_some_and(|value| value.bytes().any(|byte| byte.is_ascii_whitespace()))
        {
            return Err(VcsError::seal_failed("invalid provenance reference"));
        }
        match commit {
            Some(commit) => {
                let verb = if create_only { "create" } else { "update" };
                let _ = writeln!(transaction, "{verb} {name} {commit}");
            }
            None => {
                let _ = writeln!(transaction, "delete {name}");
            }
        }
    }
    transaction.push_str("prepare\ncommit\n");
    let output =
        super::git::git_run_stdin(path, &["update-ref", "--stdin"], transaction.as_bytes())
            .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    if !output.status.success() {
        let names = refs
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(VcsError::seal_failed(format!(
            "provenance transaction refused for {names}; existing refs retained"
        )));
    }
    Ok(())
}
