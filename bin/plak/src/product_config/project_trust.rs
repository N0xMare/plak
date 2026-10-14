//! The per-repository trust registry.
//!
//! A project's `.plak/config.toml` and `.plak/agents/` are product
//! configuration: they can name models, efforts and tool presentations, so a
//! repository the operator merely opened must not activate them. Trust is
//! recorded once per canonical project root in `trusted-projects.toml` under
//! the private product home — global authority, never inside the project —
//! and an untrusted project is skipped entirely rather than parsed.
//!
//! The file is not a credential, but it rides the same publication rules as
//! `config.toml`: validate before publish, atomic rename, synced parent.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::home::paths::PlakPaths;

use super::{AtomicStage, ConfigSaveError, atomic_file};

/// Whether `root`'s `.plak` may contribute configuration. A root that cannot
/// be canonicalized, or a registry that cannot be read, is untrusted: the
/// failure mode is "project ignored", never "project admitted".
pub(super) fn is_trusted(paths: &PlakPaths, root: &Path) -> bool {
    let Ok(root) = canonical(root) else {
        return false;
    };
    trusted_roots(paths).is_ok_and(|roots| roots.contains(&root))
}

/// Records `root` as trusted. Returns the canonical path recorded.
pub(crate) fn trust(paths: &PlakPaths, root: &Path) -> Result<PathBuf, String> {
    let root = canonical(root)?;
    let mut roots = trusted_roots(paths)?;
    if roots.insert(root.clone()) {
        publish(paths, &roots)?;
    }
    Ok(root)
}

/// Removes `root` from the registry; `Ok(false)` means it was not trusted.
pub(crate) fn revoke(paths: &PlakPaths, root: &Path) -> Result<bool, String> {
    let root = canonical(root)?;
    let mut roots = trusted_roots(paths)?;
    if !roots.remove(&root) {
        return Ok(false);
    }
    publish(paths, &roots)?;
    Ok(true)
}

/// The canonical project root, resolving symlinks so `alias/repo` and
/// `repo` are one identity.
fn canonical(root: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(root).map_err(|_| {
        format!(
            "could not resolve project root {}; it must be an existing directory",
            root.display()
        )
    })
}

fn trusted_roots(paths: &PlakPaths) -> Result<BTreeSet<PathBuf>, String> {
    let file = paths.trusted_projects_file();
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeSet::new());
        }
        Err(_) => return Err(format!("could not read {}", file.display())),
    };
    let document: toml::Value =
        toml::from_str(&text).map_err(|_| format!("{} is not valid TOML", file.display()))?;
    let mut roots = BTreeSet::new();
    let Some(entries) = document.get("roots").and_then(toml::Value::as_array) else {
        return Ok(roots);
    };
    for entry in entries {
        let Some(path) = entry.as_str() else {
            return Err(format!("{} has a non-string root", file.display()));
        };
        roots.insert(PathBuf::from(path));
    }
    Ok(roots)
}

fn publish(paths: &PlakPaths, roots: &BTreeSet<PathBuf>) -> Result<(), String> {
    // The registry lives under the product home; trusting a project must not
    // require the operator to have run `plak config init` first.
    paths.ensure_layout()?;
    let mut rendered = String::from(
        "# Plak trusted project roots (non-secret)\n\
         # A root here may contribute .plak/config.toml and .plak/agents/.\n\n",
    );
    if roots.is_empty() {
        rendered.push_str("roots = []\n");
    } else {
        rendered.push_str("roots = [\n");
        for root in roots {
            rendered.push_str(&format!(
                "  {},\n",
                toml_edit::Value::from(root.to_string_lossy().as_ref())
            ));
        }
        rendered.push_str("]\n");
    }
    let mut no_fault = |_: &Path, _: AtomicStage| Ok(());
    atomic_file(
        &paths.trusted_projects_file(),
        rendered.as_bytes(),
        false,
        &mut no_fault,
    )
    .map_err(ConfigSaveError::into_message)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn trust_then_revoke_round_trips_canonical_root() {
        let (_temp, paths) = PlakPaths::isolated_test_home();
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join("repo");
        fs::create_dir(&root).unwrap();
        assert!(!is_trusted(&paths, &root));
        let recorded = trust(&paths, &root).unwrap();
        assert_eq!(recorded, root.canonicalize().unwrap());
        assert!(is_trusted(&paths, &root));
        // A second trust is idempotent, and revoke removes exactly once.
        trust(&paths, &root).unwrap();
        assert!(revoke(&paths, &root).unwrap());
        assert!(!revoke(&paths, &root).unwrap());
        assert!(!is_trusted(&paths, &root));
    }

    #[test]
    fn missing_and_malformed_registry_fail_closed() {
        let (_temp, paths) = PlakPaths::isolated_test_home();
        let project = tempfile::tempdir().unwrap();
        assert!(!is_trusted(&paths, project.path()));
        fs::create_dir_all(paths.home()).unwrap();
        fs::write(paths.trusted_projects_file(), "roots = [").unwrap();
        assert!(!is_trusted(&paths, project.path()));
    }
}
