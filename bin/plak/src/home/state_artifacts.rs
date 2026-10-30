//! Maintenance-only physical artifact inventory, never session authority.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use plakous_hosted::workspace_maintenance as maintenance;
use serde::{Deserialize, Serialize};

use super::paths::PlakPaths;

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Artifacts {
    launch: PathBuf,
    worktrees: BTreeSet<PathBuf>,
    references: BTreeSet<String>,
    #[serde(default)]
    unadopted: BTreeSet<String>,
    #[serde(skip)]
    issue: Option<String>,
    // Invocation-local ownership evidence; never part of the tombstone encoding.
    #[serde(skip)]
    owners: BTreeMap<PathBuf, BTreeSet<String>>,
}

impl Artifacts {
    pub(super) fn owns_entry(&self, entry: &Path) -> bool {
        self.owners.keys().any(|owner| {
            owner.starts_with(entry)
                || (owner.parent() == entry.parent()
                    && owner
                        .extension()
                        .is_some_and(|extension| extension == "sqlite3")
                    && owner
                        .file_name()
                        .and_then(|name| name.to_str())
                        .zip(entry.file_name().and_then(|name| name.to_str()))
                        .is_some_and(|(database, name)| {
                            name.strip_prefix(database).is_some_and(|suffix| {
                                ["-wal", "-shm", "-journal", "-holders"].contains(&suffix)
                            })
                        }))
        })
    }

    pub(super) fn owners(&self) -> &BTreeMap<PathBuf, BTreeSet<String>> {
        &self.owners
    }

    pub(super) fn kept_reason(&self) -> String {
        if self.owners.values().flatten().all(|tree| tree == "unknown") {
            return "owns tree inventory that could not be read; retained artifacts".to_owned();
        }
        let worktrees = self
            .worktrees
            .iter()
            .filter(|path| {
                maintenance::inspect(&self.launch, std::slice::from_ref(*path), &[])
                    .is_err_and(|error| error.detail.starts_with("UNADOPTED COMMITS"))
            })
            .count();
        let references = self
            .references
            .iter()
            .filter(|reference| {
                (self.unadopted.contains(*reference)
                    && maintenance::reference_exists(&self.launch, reference).unwrap_or(false))
                    || maintenance::inspect(&self.launch, &[], std::slice::from_ref(*reference))
                        .is_err_and(|error| error.detail.starts_with("UNADOPTED COMMITS"))
            })
            .count();
        let trees = self
            .owners
            .values()
            .flatten()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|tree| {
                if tree == "unknown" {
                    "unreadable".to_owned()
                } else {
                    maintenance::digest_text(tree)[..8].to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let dirty = self
            .worktrees
            .iter()
            .filter(|path| {
                maintenance::inspect(&self.launch, std::slice::from_ref(*path), &[])
                    .is_ok_and(|dirty| dirty.into_iter().any(|dirty| dirty))
            })
            .count();
        if worktrees == 0 && references == 0 && dirty == 0 {
            return format!(
                "owns tree {trees}; ownership or repository removal could not be proved safe; retained artifacts"
            );
        }
        format!(
            "owns tree {trees} with {worktrees} unadopted worktrees / {references} unadopted refs / {dirty} dirty worktrees; retained artifacts"
        )
    }

    pub(super) fn report(
        &self,
        output: &mut dyn std::io::Write,
        forget_missing_launches: bool,
    ) -> Result<(), String> {
        let paths: Vec<_> = self.worktrees.iter().cloned().collect();
        let refs: Vec<_> = self.references.iter().cloned().collect();
        let forgetting = forget_missing_launches && self.launch_missing();
        let dirty = if forgetting {
            writeln!(output, "Owned artifacts: {} · launch missing; ownership will be forgotten unless a retained group shares its inventory; no repository artifacts removed", self.label())
                .map_err(|error| error.to_string())?;
            vec![false; paths.len()]
        } else {
            match self.check_launch().and_then(|()| {
                maintenance::inspect(&self.launch, &paths, &refs).map_err(|error| error.detail)
            }) {
                Ok(dirty) => dirty,
                Err(error) => {
                    writeln!(
                        output,
                        "Owned artifacts: {} · UNPRUNABLE; retained: {}",
                        crate::cli_output::safe_terminal_block(&self.launch.display().to_string()),
                        crate::cli_output::safe_terminal_block(&error)
                    )
                    .map_err(|error| error.to_string())?;
                    vec![false; paths.len()]
                }
            }
        };
        for (path, dirty) in paths.iter().zip(dirty) {
            writeln!(
                output,
                "Owned worktree: {}{}",
                crate::cli_output::safe_terminal_block(&path.display().to_string()),
                if dirty {
                    " · UNCOMMITTED CHANGES; retained"
                } else {
                    ""
                }
            )
            .map_err(|error| error.to_string())?;
        }
        for reference in refs {
            writeln!(
                output,
                "Owned ref: {} · {}{}",
                crate::cli_output::safe_terminal_block(&self.launch.display().to_string()),
                crate::cli_output::safe_terminal_block(&reference),
                if !forgetting
                    && self.unadopted.contains(&reference)
                    && maintenance::reference_exists(&self.launch, &reference).unwrap_or(true)
                {
                    " · UNADOPTED CANDIDATE; retained"
                } else {
                    ""
                }
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn check_launch(&self) -> Result<(), String> {
        if let Some(issue) = &self.issue {
            return Err(issue.clone());
        }
        match fs::metadata(&self.launch) {
            Err(_) if self.launch_missing() => {
                return Err("launch missing; ownership retained (use --forget-missing-launches to forget it)".to_owned());
            }
            Err(error) => {
                return Err(format!(
                    "launch ancestor missing (unmounted volume?); ownership retained: {error}"
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err("launch is not a directory; ownership retained".to_owned());
            }
            Ok(_) => {}
        }
        Ok(())
    }

    pub(super) fn launch_missing(&self) -> bool {
        self.issue.is_none()
            && self
                .launch
                .parent()
                .is_some_and(|parent| fs::metadata(parent).is_ok_and(|metadata| metadata.is_dir()))
            && fs::symlink_metadata(&self.launch)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    }

    pub(super) fn label(&self) -> String {
        crate::cli_output::safe_terminal_block(&self.launch.display().to_string())
    }

    pub(super) fn check_clean(&self) -> Result<(), String> {
        self.check_launch()?;
        let paths: Vec<_> = self.worktrees.iter().cloned().collect();
        let refs: Vec<_> = self.references.iter().cloned().collect();
        let dirty =
            maintenance::inspect(&self.launch, &paths, &refs).map_err(|error| error.detail)?;
        for reference in &self.unadopted {
            if maintenance::reference_exists(&self.launch, reference)
                .map_err(|error| error.detail)?
            {
                return Err(format!(
                    "unadopted candidate; artifact group retained: {reference}"
                ));
            }
        }
        if let Some((path, _)) = paths.iter().zip(dirty).find(|(_, dirty)| *dirty) {
            return Err(format!(
                "uncommitted changes; worktree retained: {}",
                path.display()
            ));
        }
        Ok(())
    }

    pub(super) fn remove(&self) -> Result<(), String> {
        self.check_launch()?;
        maintenance::remove(
            &self.launch,
            &self.worktrees.iter().cloned().collect::<Vec<_>>(),
            &self.references.iter().cloned().collect::<Vec<_>>(),
        )
        .map_err(|error| error.detail)
    }
}

// Resolve missing suffixes without treating inaccessible or linked paths as absent.
fn canonicalize_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for name in suffix.into_iter().rev() {
                    canonical.push(name);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(ancestor) {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => return Err(error),
                    Err(other) => return Err(other),
                }
                let Some(name) = ancestor.file_name() else {
                    return Err(error);
                };
                suffix.push(name);
                ancestor = ancestor.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn add(groups: &mut BTreeMap<PathBuf, Artifacts>, mut artifact: Artifacts) {
    if artifact.issue.is_none() {
        match fs::canonicalize(&artifact.launch) {
            Ok(launch) => {
                artifact.launch = launch;
                let mut worktrees = BTreeSet::new();
                for path in &artifact.worktrees {
                    if [
                        Some(path.as_path()),
                        path.parent(),
                        path.parent().and_then(Path::parent),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|ancestor| {
                        fs::symlink_metadata(ancestor)
                            .is_ok_and(|metadata| metadata.file_type().is_symlink())
                    }) {
                        artifact.issue =
                            Some(format!("linked worktree path {}; retained", path.display()));
                        continue;
                    }
                    let canonical = canonicalize_missing(path);
                    match canonical {
                        Ok(path) => {
                            worktrees.insert(path);
                        }
                        Err(error) => {
                            artifact.issue = Some(format!(
                                "unresolvable worktree {}; retained: {error}",
                                path.display()
                            ));
                        }
                    }
                }
                if artifact.issue.is_none() {
                    artifact.worktrees = worktrees;
                }
            }
            // Keep the original names so explicit forgetting can identify ENOENT.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                artifact.issue = Some(format!(
                    "launch ancestor missing (unmounted volume?); unresolvable launch; ownership retained: {error}"
                ));
            }
        }
    }
    let group = groups
        .entry(artifact.launch.clone())
        .or_insert_with(|| Artifacts {
            launch: artifact.launch.clone(),
            ..Artifacts::default()
        });
    group.worktrees.extend(artifact.worktrees);
    group.references.extend(artifact.references);
    group.unadopted.extend(artifact.unadopted);
    for (path, trees) in artifact.owners {
        group.owners.entry(path).or_default().extend(trees);
    }
    if let Some(issue) = artifact.issue
        && group.issue.as_ref().is_none_or(|current| issue < *current)
    {
        group.issue = Some(issue);
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    matches!((fs::canonicalize(left), fs::canonicalize(right)), (Ok(left), Ok(right)) if left == right)
}

/// Best-effort, read-only diagnosis. Unknown copies are never removal targets.
pub(super) fn unowned_worktrees(paths: &PlakPaths) -> (Vec<PathBuf>, bool) {
    let mut incomplete = false;
    let mut roots = BTreeSet::from([paths.home().join("worktrees")]);
    if let Some(project) = paths.project_root() {
        roots.insert(project.join(".plak/worktrees"));
    }
    let mut owned = BTreeSet::new();
    let mut record = |artifact: Artifacts| {
        roots.insert(artifact.launch.join(".plak/worktrees"));
        owned.extend(
            artifact
                .worktrees
                .into_iter()
                .map(|path| canonicalize_missing(&path).unwrap_or(path)),
        );
    };
    let mut pending = vec![paths.state_root()];
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                incomplete |= error.kind() != std::io::ErrorKind::NotFound;
                continue;
            }
        };
        if metadata.is_dir() {
            match fs::read_dir(path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => pending.push(entry.path()),
                            Err(error) => {
                                incomplete |= error.kind() != std::io::ErrorKind::NotFound;
                            }
                        }
                    }
                }
                Err(error) => incomplete |= error.kind() != std::io::ErrorKind::NotFound,
            }
        } else if metadata.is_file() {
            if path
                .file_name()
                .is_some_and(|name| name == "subagents.sqlite3")
            {
                let artifacts = rusqlite::Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .map_err(|error| error.to_string())
                .and_then(|database| tree_artifacts(&database));
                match artifacts {
                    Ok(artifacts) => {
                        for (_, artifact) in artifacts {
                            record(artifact);
                        }
                    }
                    Err(_) => incomplete = true,
                }
            } else if path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "deleted-artifacts")
                && tombstone_id(&path).is_some()
            {
                match fs::read(path) {
                    Ok(bytes) => match serde_json::from_slice(&bytes) {
                        Ok(artifact) => record(artifact),
                        Err(_) => incomplete = true,
                    },
                    Err(_) => incomplete = true,
                }
            }
        }
    }
    let mut unowned = BTreeSet::new();
    for root in roots {
        if !fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && !owned.contains(&canonicalize_missing(&path).unwrap_or_else(|_| path.clone()))
            {
                unowned.insert(path);
            }
        }
    }
    (unowned.into_iter().collect(), incomplete)
}

pub(super) fn sessions(
    databases: &[(PathBuf, rusqlite::Connection)],
    paths: &PlakPaths,
) -> Result<BTreeSet<String>, String> {
    let Some((_, database)) = databases
        .iter()
        .find(|(path, _)| same_file(path, &paths.product_database()))
    else {
        match fs::symlink_metadata(paths.product_database()) {
            Ok(_) => return Err("current product database identity cannot be proved under maintenance locks; nothing removed".to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        if paths
            .subagents_database()
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            return Err("current subagents database requires the product database under the same maintenance locks; nothing removed".to_owned());
        }
        return Ok(BTreeSet::new());
    };
    let mut statement = database
        .prepare("SELECT id, source_session_id FROM sessions")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(rows
        .into_iter()
        .flat_map(|(id, source)| std::iter::once(id).chain(source))
        .collect())
}

fn tree_artifacts(database: &rusqlite::Connection) -> Result<Vec<(String, Artifacts)>, String> {
    let version = database
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|error| error.to_string())?;
    if !(1..=plakous_recursive_subagents_store::SCHEMA_VERSION).contains(&version) {
        return Err(format!("unsupported tree schema {version}"));
    }
    let mut statement = database
        .prepare("SELECT tree_id, launch_checkout FROM agent_trees")
        .map_err(|error| format!("cannot inventory owned tree artifacts: {error}"))?;
    let trees = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut artifacts = Vec::new();
    for (tree, launch) in trees {
        let mut artifact = Artifacts {
            launch,
            ..Artifacts::default()
        };
        let mut workspaces = database
            .prepare("SELECT path FROM agent_workspaces WHERE tree_id=?1 AND mode!='inherit'")
            .map_err(|error| error.to_string())?;
        artifact.worktrees = workspaces
            .query_map([&tree], |row| row.get::<_, String>(0).map(PathBuf::from))
            .map_err(|error| error.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|error| error.to_string())?;
        if version < 9 {
            for path in &artifact.worktrees {
                if let Some(id) = path.file_name().and_then(|id| id.to_str()) {
                    artifact.references.insert(format!("refs/heads/plak/{id}"));
                }
            }
        } else {
            let mut branches = database
                .prepare("SELECT branch FROM agent_nodes WHERE tree_id=?1 AND branch IS NOT NULL AND workspace_id IN (SELECT workspace_id FROM agent_workspaces WHERE tree_id=?1 AND mode!='inherit')")
                .map_err(|error| error.to_string())?;
            for branch in branches
                .query_map([&tree], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
            {
                let branch = branch.map_err(|error| error.to_string())?;
                if branch.starts_with("plak/") {
                    artifact.references.insert(format!("refs/heads/{branch}"));
                }
            }
            let mut owners = database.prepare("SELECT agent_id FROM agent_nodes WHERE tree_id=?1 AND workspace_id IN (SELECT workspace_id FROM agent_workspaces WHERE tree_id=?1 AND mode!='inherit')")
                .map_err(|error| error.to_string())?;
            for owner in owners
                .query_map([&tree], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
            {
                let agent = owner.map_err(|error| error.to_string())?;
                artifact.references.insert(maintenance::copy_base_ref(
                    &maintenance::digest_text(&tree),
                    &agent,
                ));
            }
        }
        let mut candidates = database
            .prepare("SELECT candidate_id FROM agent_candidates WHERE tree_id=?1")
            .map_err(|error| error.to_string())?;
        let namespaced = database
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .map_err(|error| error.to_string())?
            >= 3;
        artifact.references.extend(
            candidates
                .query_map([&tree], |row| {
                    let id = row.get::<_, String>(0)?;
                    Ok(if namespaced {
                        maintenance::candidate_ref(&maintenance::digest_text(&tree), &id)
                    } else {
                        format!("refs/plak/candidates/{id}")
                    })
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?,
        );
        if version >= 9 {
            for candidate in candidates
                .query_map([&tree], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
            {
                let candidate = candidate.map_err(|error| error.to_string())?;
                artifact.references.extend(maintenance::candidate_refs(
                    &maintenance::digest_text(&tree),
                    &candidate,
                ));
            }
        }
        let has_adopted_by = database
            .prepare("PRAGMA table_info(agent_candidates)")
            .map_err(|error| error.to_string())?
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .iter()
            .any(|column| column == "adopted_by");
        if version >= 9 {
            let mut cleanups = database.prepare("SELECT canonical_request FROM agent_operations WHERE tree_id=?1 AND tool_name IN ('candidate_unpin','rescue_pin')")
                .map_err(|error| error.to_string())?;
            for cleanup in cleanups
                .query_map([&tree], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
            {
                let encoded = cleanup.map_err(|error| error.to_string())?;
                let ids: Vec<String> = serde_json::from_str(&encoded)
                    .map_err(|_| "invalid candidate cleanup intent")?;
                for id in ids {
                    artifact.references.extend(maintenance::candidate_refs(
                        &maintenance::digest_text(&tree),
                        &id,
                    ));
                }
            }
        }
        let sql = if version >= 9 {
            "SELECT c.candidate_id FROM agent_candidates c WHERE c.tree_id=?1 AND ((c.adopted_by IS NULL AND c.adopted_via IS NULL) OR EXISTS (SELECT 1 FROM agent_workspaces w WHERE w.tree_id=c.tree_id AND w.status NOT IN ('discarded','clean_removed') AND ((c.kind='conflict' AND w.input_candidate_id=c.candidate_id) OR w.resolves=c.candidate_id)))"
        } else if has_adopted_by {
            "SELECT candidate_id FROM agent_candidates WHERE tree_id=?1 AND adopted_by IS NULL"
        } else {
            "SELECT candidate_id FROM agent_candidates WHERE tree_id=?1"
        };
        artifact.unadopted = database
            .prepare(sql)
            .map_err(|error| error.to_string())?
            .query_map([&tree], |row| {
                let id = row.get::<_, String>(0)?;
                Ok(if namespaced {
                    maintenance::candidate_ref(&maintenance::digest_text(&tree), &id)
                } else {
                    format!("refs/plak/candidates/{id}")
                })
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|error| error.to_string())?;
        artifacts.push((tree, artifact));
    }
    Ok(artifacts)
}

/// All databases and holder proofs have already been locked by prune-legacy.
/// Only the physical names in maintenance columns are read from foreign trees.
pub(super) fn inventory(
    paths: &PlakPaths,
    databases: &[(PathBuf, rusqlite::Connection)],
    sessions: &BTreeSet<String>,
) -> Result<(Vec<Artifacts>, BTreeMap<String, Artifacts>), String> {
    #[cfg(test)]
    tests::before_inventory();
    if paths
        .subagents_database()
        .try_exists()
        .map_err(|error| error.to_string())?
        && !databases
            .iter()
            .any(|(path, _)| same_file(path, &paths.subagents_database()))
    {
        return Err("current subagents database identity cannot be proved under maintenance locks; nothing removed".to_owned());
    }
    let mut selected = BTreeMap::new();
    let mut protected = BTreeMap::new();
    let mut carried = BTreeMap::new();
    for (path, database) in databases.iter().filter(|(path, _)| {
        path.file_name()
            .is_some_and(|name| name == "subagents.sqlite3")
    }) {
        if let Err(error) = fs::canonicalize(path) {
            add(
                &mut selected,
                Artifacts {
                    launch: path.clone(),
                    issue: Some(format!("database identity unresolvable; retained: {error}")),
                    owners: BTreeMap::from([(
                        path.clone(),
                        BTreeSet::from(["unknown".to_owned()]),
                    )]),
                    ..Artifacts::default()
                },
            );
            continue;
        }
        let current = same_file(path, &paths.subagents_database());
        let result = tree_artifacts(database);
        if let Ok(artifacts) = &result {
            for (tree, artifact) in artifacts {
                let mut artifact = artifact.clone();
                artifact
                    .owners
                    .entry(path.clone())
                    .or_default()
                    .insert(tree.clone());
                // Current rows are protected regardless of product-row presence.
                if current || sessions.contains(tree) {
                    if !current {
                        carried.insert(tree.clone(), artifact.clone());
                    }
                    add(&mut protected, artifact);
                } else {
                    add(&mut selected, artifact);
                }
            }
        }
        if let Err(error) = result {
            if current {
                return Err(format!(
                    "current tree inventory unavailable; nothing removed: {error}"
                ));
            }
            add(
                &mut selected,
                Artifacts {
                    launch: path.clone(),
                    issue: Some(format!("cannot inventory owned tree artifacts: {error}")),
                    owners: BTreeMap::from([(
                        path.clone(),
                        BTreeSet::from(["unknown".to_owned()]),
                    )]),
                    ..Artifacts::default()
                },
            );
        }
    }
    for state in std::iter::once(paths.state_dir())
        .chain(super::state::other_state(paths))
        .filter(|state| state.is_dir())
    {
        let dir = state.join("deleted-artifacts");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        for entry in entries {
            let path = entry.map_err(|error| error.to_string())?.path();
            let Some(id) = tombstone_id(&path) else {
                continue;
            };
            let observed = fs::read(&path)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    let mut artifact = serde_json::from_slice::<Artifacts>(&bytes)
                        .map_err(|error| error.to_string())?;
                    artifact
                        .owners
                        .insert(path.clone(), BTreeSet::from([id.clone()]));
                    Ok(artifact)
                });
            match observed {
                Ok(artifact) if sessions.contains(&id) => {
                    if state != paths.state_dir() {
                        carried.insert(id, artifact.clone());
                    }
                    add(&mut protected, artifact);
                }
                Ok(artifact) => add(&mut selected, artifact),
                Err(error) => add(
                    &mut selected,
                    Artifacts {
                        launch: path.clone(),
                        issue: Some(format!("unreadable deleted artifact inventory: {error}")),
                        owners: BTreeMap::from([(path.clone(), BTreeSet::from([id]))]),
                        ..Artifacts::default()
                    },
                ),
            }
        }
    }
    for artifact in selected.values_mut() {
        for current in protected
            .values()
            .filter(|current| current.issue.is_some() || fs::canonicalize(&current.launch).is_err())
        {
            if matches!((canonicalize_missing(&artifact.launch), canonicalize_missing(&current.launch)), (Ok(selected), Ok(protected)) if selected != protected)
            {
                continue;
            }
            artifact.issue = Some(format!(
                "protected launch {} has unresolved ownership paths; possibly aliasing group retained",
                current.launch.display()
            ));
            break;
        }
        if let Some(current) = protected.get(&artifact.launch) {
            artifact
                .worktrees
                .retain(|path| !current.worktrees.contains(path));
            artifact
                .references
                .retain(|reference| !current.references.contains(reference));
            artifact
                .unadopted
                .retain(|reference| !current.references.contains(reference));
        }
    }
    Ok((
        selected
            .into_values()
            .filter(|artifact| {
                artifact.issue.is_some()
                    || !artifact.worktrees.is_empty()
                    || !artifact.references.is_empty()
            })
            .collect(),
        carried,
    ))
}

/// Retains physical ownership before deleting its only tree rows. A partial
/// session delete can retry this write, and maintenance never selects the
/// inventory while its current session still exists.
pub(crate) fn remember_deleted_tree(paths: &PlakPaths, tree_id: &str) -> Result<(), String> {
    let observed = plakous_recursive_subagents_store::SqliteTreeStore::inspect(
        &paths.subagents_database(),
        tree_id,
    )
    .map_err(|error| error.message().to_owned())?;
    let plakous_recursive_subagents_store::TreeInspection::Tree(tree) = observed else {
        return Ok(());
    };
    let rows = tree.rows();
    let mut artifact = Artifacts {
        launch: rows.launch_checkout.clone(),
        ..Artifacts::default()
    };
    for workspace in rows
        .workspaces()
        .values()
        .filter(|row| row.mode != plakous_host_protocol::topology::WorkspaceMode::Inherit)
    {
        for node in rows
            .nodes()
            .values()
            .filter(|node| node.workspace_id == workspace.workspace_id)
        {
            artifact.references.insert(maintenance::copy_base_ref(
                &maintenance::digest_text(tree_id),
                &node.agent_id,
            ));
            if let Some(branch) = node
                .branch
                .as_ref()
                .filter(|branch| branch.starts_with("plak/"))
            {
                artifact.references.insert(format!("refs/heads/{branch}"));
            }
        }
        artifact.worktrees.insert(workspace.path.clone());
    }
    for candidate in rows.candidates().values() {
        let reference = maintenance::candidate_ref(
            &maintenance::digest_text(tree_id),
            candidate.candidate_id.as_str(),
        );
        if candidate.adopted_by.is_none() && candidate.adopted_via.is_none() {
            artifact.unadopted.insert(reference.clone());
        }
        artifact.references.extend(maintenance::candidate_refs(
            &maintenance::digest_text(tree_id),
            candidate.candidate_id.as_str(),
        ));
    }
    for cleanup in rows
        .operations()
        .values()
        .filter(|op| matches!(op.tool_name.as_str(), "candidate_unpin" | "rescue_pin"))
    {
        let ids: Vec<String> = serde_json::from_str(&cleanup.canonical_request)
            .map_err(|_| "invalid candidate cleanup intent")?;
        for id in ids {
            artifact.references.extend(maintenance::candidate_refs(
                &maintenance::digest_text(tree_id),
                &id,
            ));
        }
    }
    remember_artifacts(paths, tree_id, artifact)
}

pub(super) fn remember_artifacts(
    paths: &PlakPaths,
    tree_id: &str,
    mut artifact: Artifacts,
) -> Result<(), String> {
    let id = uuid::Uuid::parse_str(tree_id).map_err(|_| "invalid deleted session id")?;
    if id.to_string() != tree_id {
        return Err("deleted session id must be a canonical lowercase hyphenated UUID".to_owned());
    }
    let dir = paths.state_dir().join("deleted-artifacts");
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)
        .map_err(|error| error.to_string())?;
    let path = dir.join(format!("{tree_id}.json"));
    if path.exists() {
        let previous: Artifacts =
            serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if !same_file(&previous.launch, &artifact.launch) && previous.launch != artifact.launch {
            return Err("deleted artifact launch changed; retained for inspection".to_owned());
        }
        artifact.worktrees.extend(previous.worktrees);
        artifact.references.extend(previous.references);
        artifact.unadopted.extend(previous.unadopted);
    }
    let bytes = serde_json::to_vec(&artifact).map_err(|error| error.to_string())?;
    let mut scratch = tempfile::NamedTempFile::new_in(&dir).map_err(|error| error.to_string())?;
    scratch
        .as_file_mut()
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    scratch
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    scratch.persist(path).map_err(|error| error.to_string())?;
    fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())
}

pub(super) fn forget_deleted_inventory(
    paths: &PlakPaths,
    databases: &[(PathBuf, rusqlite::Connection)],
    retained: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    let current = sessions(databases, paths)?;
    let dir = paths.state_dir().join("deleted-artifacts");
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if let Some(id) = tombstone_id(&path)
            && !current.contains(&id)
            && !retained.contains(&path)
        {
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn tombstone_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.strip_suffix(".json")?;
    let id = uuid::Uuid::parse_str(name).ok()?;
    (id.to_string() == name).then(|| name.to_owned())
}

#[cfg(test)]
mod tests;
