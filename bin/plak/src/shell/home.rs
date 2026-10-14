//! The one `~/.plak` and XDG-state resolution per process.
//!
//! `PlakPaths::from_env()` and `application_state_home()` are policy the
//! binary owns. `Home` reads the environment once at CLI entry and hands
//! out borrows.
//!
//! Each half keeps its own resolution result rather than being collapsed into
//! one fallible constructor: a command that needs only the product home still
//! fails with the product-home message, and a command that needs only the
//! state home still fails with the state-home message.

use std::path::{Path, PathBuf};

use crate::home::paths::PlakPaths;

use crate::{CliFailure, CliResult};

/// The product home and the Plakous state home, resolved once.
#[derive(Debug)]
pub struct Home {
    paths: Result<PlakPaths, String>,
    state: Result<PathBuf, String>,
}

impl Home {
    #[cfg(test)]
    pub(crate) fn explicit(paths: PlakPaths, state: PathBuf) -> Self {
        Self {
            paths: Ok(paths),
            state: Ok(state),
        }
    }

    /// Reads `PLAK_HOME`/`HOME` and `XDG_STATE_HOME`/`HOME` once. Neither
    /// failure is raised here; a command that needs a half asks for it.
    pub(crate) fn from_env() -> Self {
        Self {
            paths: PlakPaths::from_env(),
            state: crate::home::paths::application_state_home(),
        }
    }

    /// The `~/.plak` layout, or the resolution failure that command reports.
    pub(crate) fn paths(&self) -> CliResult<&PlakPaths> {
        self.paths
            .as_ref()
            .map_err(|error| CliFailure::from(error.clone()))
    }

    /// The `~/.plak` layout when it resolved, for the best-effort readers
    /// (diagnostics install, credential telemetry) that degrade without it.
    pub(crate) fn paths_if_resolved(&self) -> Option<&PlakPaths> {
        self.paths.as_ref().ok()
    }

    /// The XDG state home Plakous native subscriptions live under.
    pub(crate) fn state(&self) -> CliResult<&Path> {
        self.state
            .as_deref()
            .map_err(|error| CliFailure::new(error.clone()))
    }
}
