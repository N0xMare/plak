use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use crate::home::paths::PlakPaths;

type Fingerprint = (u64, u64, u64, i64, i64);
type Files = BTreeMap<PathBuf, Option<Fingerprint>>;

pub(super) struct Inputs {
    global: Files,
    workspaces: BTreeMap<PathBuf, Files>,
}

impl Inputs {
    pub(super) fn new(paths: &PlakPaths) -> Self {
        Self {
            global: global(paths),
            workspaces: BTreeMap::new(),
        }
    }

    pub(super) fn global_changed(&mut self, paths: &PlakPaths) -> bool {
        replace(&mut self.global, global(paths))
    }

    pub(super) fn remember_workspace(&mut self, paths: &PlakPaths, workspace: Option<&Path>) {
        if let Some(workspace) = workspace {
            self.workspaces
                .insert(workspace.to_owned(), project(paths, workspace));
        }
    }

    pub(super) fn workspace_changed(
        &mut self,
        paths: &PlakPaths,
        workspace: Option<&Path>,
    ) -> bool {
        workspace.is_some_and(|workspace| {
            let next = project(paths, workspace);
            self.workspaces
                .get_mut(workspace)
                .is_none_or(|previous| replace(previous, next))
        })
    }
}

fn replace(previous: &mut Files, next: Files) -> bool {
    if *previous == next {
        return false;
    }
    *previous = next;
    true
}

fn stamp(files: &mut Files, path: PathBuf) {
    let value = path.metadata().ok().map(|meta| {
        (
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
        )
    });
    files.insert(path, value);
}

fn directory(files: &mut Files, path: PathBuf, extension: &str) {
    if let Ok(entries) = path.read_dir() {
        for child in entries.flatten().map(|entry| entry.path()) {
            if child.extension().is_some_and(|ext| ext == extension) {
                stamp(files, child);
            }
        }
    }
    stamp(files, path);
}

fn global(paths: &PlakPaths) -> Files {
    let mut files = BTreeMap::new();
    stamp(&mut files, paths.config_file());
    stamp(&mut files, paths.trusted_projects_file());
    directory(&mut files, paths.user_agents_dir(), "toml");
    files
}

fn project(paths: &PlakPaths, workspace: &Path) -> Files {
    let mut files = BTreeMap::new();
    let paths = paths.for_workspace(workspace);
    if let Some(config) = paths.project_config_file() {
        stamp(&mut files, config);
    }
    if let Some(agents) = paths.project_agents_dir() {
        directory(&mut files, agents, "toml");
    }
    files
}
