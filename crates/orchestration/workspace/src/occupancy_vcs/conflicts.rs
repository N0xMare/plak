//! Pin textual conflicts without landing them, and fence unresolved resolvers.

use super::{
    AdoptFailure, AdoptRequest, ConflictCandidate, ConflictPath, VcsSnapshot, git, namespace, pins,
};

pub(super) fn check_resolver(request: &AdoptRequest<'_>) -> Result<(), AdoptFailure> {
    if request.accept_conflicts || request.resolver_paths.is_empty() {
        return Ok(());
    }
    let mut args = vec![
        "--literal-pathspecs",
        "grep",
        "-l",
        "-z",
        "-I",
        "-E",
        "^(<<<<<<<|>>>>>>>)( |$)",
        request.candidate.commit.as_str(),
        "--",
    ];
    args.extend(request.resolver_paths.iter().map(String::as_str));
    let output = git::git_query(request.target_path, &args)
        .map_err(|error| AdoptFailure::Unavailable(error.to_string()))?;
    if output.status.code() == Some(1) {
        return Ok(());
    }
    if !output.status.success() {
        return Err(AdoptFailure::Unavailable(
            "cannot inspect resolver conflict markers".into(),
        ));
    }
    let prefix = format!("{}:", request.candidate.commit);
    let paths = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| ConflictPath {
            path: path.strip_prefix(&prefix).unwrap_or(path).to_owned(),
            kind: "unresolved markers".into(),
        })
        .collect();
    Err(AdoptFailure::Conflict {
        paths,
        candidate: None,
    })
}

pub(super) fn pin_conflict(
    request: &AdoptRequest<'_>,
    target: &VcsSnapshot,
    stdout: &str,
) -> Result<AdoptFailure, AdoptFailure> {
    let paths = git::parse_merge_tree_conflicts(stdout);
    let textual = !paths.is_empty()
        && paths
            .iter()
            .all(|path| matches!(path.kind.as_str(), "contents" | "add/add"));
    let candidate = if let Some(pin) = request.conflict_pin.filter(|_| textual) {
        let tree = stdout.split('\0').next().unwrap_or_default().trim();
        if tree.is_empty() || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(AdoptFailure::Unavailable(
                "conflicted merge produced no tree".into(),
            ));
        }
        let _guard = namespace::reserve(
            &pins::common(request.target_path)
                .map_err(|error| AdoptFailure::Unavailable(error.detail))?,
            request.namespace,
        )
        .map_err(|error| AdoptFailure::Unavailable(error.detail))?;
        let commit = git::commit_tree(request.target_path, tree, &target.commit, &pin.label)
            .map_err(|error| AdoptFailure::Unavailable(error.detail))?;
        pins::update_reserved(
            request.target_path,
            &[
                (
                    pins::reference(request.namespace, "candidates", &pin.id),
                    Some(commit.as_str()),
                ),
                (
                    pins::reference(request.namespace, "candidate-bases", &pin.id),
                    Some(target.commit.as_str()),
                ),
            ],
            false,
        )
        .map_err(|error| AdoptFailure::Unavailable(error.detail))?;
        Some(Box::new(ConflictCandidate {
            snapshot: VcsSnapshot {
                commit,
                content: tree.to_owned(),
                ..VcsSnapshot::default()
            },
            base: target.clone(),
        }))
    } else {
        None
    };
    Ok(AdoptFailure::Conflict { paths, candidate })
}
