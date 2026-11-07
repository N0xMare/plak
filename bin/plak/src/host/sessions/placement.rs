//! Runner-owned workspace validation; no remote path is inspected on the host.

use std::path::PathBuf;
#[cfg(test)]
use std::path::{Component, Path};

#[cfg(test)]
use plakous_host_protocol::wire::{PlacementReason, WorkspaceRefusalKind};
use plakous_host_protocol::wire::{RequestRefusal, RunnerLabel};

/// Product/runner seam: validation returns the exact path bytes before publication.
pub(crate) trait Placement: Send + Sync {
    fn validate(&self, runner: &RunnerLabel, workspace: &str) -> Result<Validated, RequestRefusal>;
}

pub(crate) struct Validated {
    pub workspace: PathBuf,
    pub runner: Option<plakous_hosted::sessions::RunnerDisplay>,
    pub probe: std::sync::Arc<dyn plakous::tools::WorkspaceProbe>,
}

pub(super) struct RegistryPlacement {
    pub services: crate::shell::services::Services,
    pub runtime: Option<tokio::runtime::Handle>,
}

impl Placement for RegistryPlacement {
    fn validate(&self, runner: &RunnerLabel, workspace: &str) -> Result<Validated, RequestRefusal> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| RequestRefusal::SessionUnavailable {
                detail: "the host runner executor is unavailable".to_owned(),
            })?;
        let registry = self.services.runners();
        let bound = runtime
            .block_on(registry.bind_session_root(&runner.runner_id, workspace))
            .map_err(|failure| super::runner_views::refusal(runner, failure, &registry.rows()))?;
        let selected = super::runner_views::label(&bound.runner_id, &registry.rows());
        let display =
            (bound.runner_id != "local").then(|| plakous_hosted::sessions::RunnerDisplay {
                name: selected.name,
                runner_id: bound.runner_id,
                workspace_root: bound.root.clone(),
                workspace_cwd: bound.cwd,
            });
        let probe = bound.services.map_or_else(
            || {
                std::sync::Arc::new(plakous::tools::LocalWorkspaceProbe)
                    as std::sync::Arc<dyn plakous::tools::WorkspaceProbe>
            },
            |services| services.workspace_probe(),
        );
        Ok(Validated {
            workspace: PathBuf::from(bound.root),
            runner: display,
            probe,
        })
    }
}

/// Isolated test authority; production always uses the paired registry.
#[cfg(test)]
pub(super) struct LocalPlacement {
    pub product_home: PathBuf,
}

#[cfg(test)]
impl Placement for LocalPlacement {
    fn validate(&self, runner: &RunnerLabel, workspace: &str) -> Result<Validated, RequestRefusal> {
        self.local_validate(runner, workspace)
            .map_err(|reason| RequestRefusal::PlacementRefused {
                runner: runner.clone(),
                reason: Box::new(reason),
            })
    }
}

#[cfg(test)]
impl LocalPlacement {
    fn local_validate(
        &self,
        runner: &RunnerLabel,
        workspace: &str,
    ) -> Result<Validated, PlacementReason> {
        if runner.runner_id != "local" || runner.name != "local" {
            return Err(PlacementReason::RunnerNotConfigured {
                saved_id: runner.runner_id.clone(),
                alias: Some(runner.name.clone()),
                current_id: None,
            });
        }
        let path = Path::new(workspace);
        let refused = |kind| PlacementReason::WorkspaceRefused {
            kind,
            canonical: None,
        };
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(refused(WorkspaceRefusalKind::InvalidPath));
        }
        // Resolve existing ancestors before the existence check, including a
        // symlink whose missing suffix escapes into a protected product home.
        let mut ancestor = path;
        let mut suffix = Vec::new();
        let resolved = loop {
            match std::fs::canonicalize(ancestor) {
                Ok(mut resolved) => {
                    for component in suffix.iter().rev() {
                        resolved.push(component);
                    }
                    break resolved;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let Some(name) = ancestor.file_name() else {
                        return Err(refused(WorkspaceRefusalKind::NotFound));
                    };
                    suffix.push(name);
                    let Some(parent) = ancestor.parent() else {
                        return Err(refused(WorkspaceRefusalKind::InvalidPath));
                    };
                    ancestor = parent;
                }
                Err(error) => {
                    return Err(refused(
                        if error.kind() == std::io::ErrorKind::PermissionDenied {
                            WorkspaceRefusalKind::PermissionDenied
                        } else {
                            WorkspaceRefusalKind::InvalidPath
                        },
                    ));
                }
            }
        };
        let home =
            std::fs::canonicalize(&self.product_home).unwrap_or_else(|_| self.product_home.clone());
        if resolved.starts_with(&home) || home.starts_with(&resolved) {
            return Err(refused(WorkspaceRefusalKind::ProductHome));
        }
        let metadata = std::fs::metadata(&resolved).map_err(|error| {
            refused(match error.kind() {
                std::io::ErrorKind::NotFound => WorkspaceRefusalKind::NotFound,
                std::io::ErrorKind::PermissionDenied => WorkspaceRefusalKind::PermissionDenied,
                _ => WorkspaceRefusalKind::InvalidPath,
            })
        })?;
        if !metadata.is_dir() {
            return Err(refused(WorkspaceRefusalKind::NotDirectory));
        }
        let canonical = resolved
            .to_str()
            .ok_or_else(|| refused(WorkspaceRefusalKind::InvalidPath))?;
        if canonical != workspace {
            return Err(PlacementReason::WorkspaceRefused {
                kind: WorkspaceRefusalKind::NotCanonical,
                canonical: Some(canonical.to_owned()),
            });
        }
        Ok(Validated {
            workspace: resolved,
            runner: None,
            probe: std::sync::Arc::new(plakous::tools::LocalWorkspaceProbe),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn local_validation_does_not_guess_remote_authority_or_rewrite_create_bytes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(root.path()).unwrap();
        let placement = LocalPlacement {
            product_home: workspace.join("home"),
        };
        std::fs::create_dir(workspace.join("work")).unwrap();
        let path = workspace.join("work");
        assert!(
            placement
                .local_validate(&RunnerLabel::local(), path.to_str().unwrap())
                .is_ok()
        );
        let remote = RunnerLabel {
            name: "machine".into(),
            runner_id: "pinned-id".into(),
        };
        assert!(matches!(
            placement.local_validate(&remote, path.to_str().unwrap()),
            Err(PlacementReason::RunnerNotConfigured { .. })
        ));
        assert!(matches!(
            placement.local_validate(&RunnerLabel::local(), "relative"),
            Err(PlacementReason::WorkspaceRefused {
                kind: WorkspaceRefusalKind::InvalidPath,
                ..
            })
        ));
        #[cfg(unix)]
        {
            let alias = workspace.join("alias");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            assert!(
                matches!(placement.local_validate(&RunnerLabel::local(), alias.to_str().unwrap()), Err(PlacementReason::WorkspaceRefused { kind: WorkspaceRefusalKind::NotCanonical, canonical: Some(ref canonical) }) if canonical == path.to_str().unwrap())
            );
        }
    }
}
