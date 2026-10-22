//! Fresh host construction material and its bounded reconstructive path policy.

use super::{ExecutionEnvironmentError, ExecutionEnvironmentErrorKind};
use std::path::{Component, Path, PathBuf};

/// Host process inputs supplied by the embedding application.
///
/// The optional required PATH prefix is workspace-relative and reconstructive.
/// It grants no additional filesystem authority or environment delegation.
#[derive(Clone, Debug, Default)]
pub struct HostProcessOptions {
    pub(super) required_path_prefix: Option<String>,
    pub(super) discovery_ceiling: bool,
}

impl HostProcessOptions {
    /// Validates an optional required workspace prefix.
    ///
    /// # Errors
    /// Rejects a prefix which is not a nonempty relative path without traversal,
    /// `:` or NUL, or which is non-UTF-8 or oversized.
    pub fn new(required_path_prefix: Option<PathBuf>) -> Result<Self, ExecutionEnvironmentError> {
        let prefix = required_path_prefix
            .map(|path| {
                if !valid_path(&path, false) || path.to_str().is_some_and(|text| text.contains(':'))
                {
                    return Err(invalid_options());
                }
                path.into_os_string()
                    .into_string()
                    .map_err(|_| invalid_options())
            })
            .transpose()?;
        Ok(Self {
            required_path_prefix: prefix,
            discovery_ceiling: false,
        })
    }

    /// Stops inherited Git repository discovery at the workspace root's parent.
    /// This is a discovery guard, not containment against explicit repository paths.
    #[must_use]
    pub fn with_discovery_ceiling(mut self, enabled: bool) -> Self {
        self.discovery_ceiling = enabled;
        self
    }
}

pub(super) fn validate_prefix(prefix: Option<&str>) -> Result<(), ExecutionEnvironmentError> {
    if prefix.is_some_and(|path| !valid_path(Path::new(path), false) || path.contains(':')) {
        return Err(invalid_options());
    }
    Ok(())
}

fn valid_path(path: &Path, absolute: bool) -> bool {
    path.is_absolute() == absolute
        && path
            .to_str()
            .is_some_and(|text| !text.is_empty() && text.len() <= 4096 && !text.contains('\0'))
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::RootDir))
        && path
            .components()
            .any(|component| matches!(component, Component::Normal(_)))
}

fn invalid_options() -> ExecutionEnvironmentError {
    ExecutionEnvironmentError::new(
        ExecutionEnvironmentErrorKind::Workspace,
        "host process required prefix is invalid",
    )
}
