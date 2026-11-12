//! Runner-owned admission and curated shell context for operator terminals.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous::runner::operator::{OperatorAccountFallback, OperatorWorkspace};
use plakous::runner::protocol::operator::OperatorRefusal;

use super::{CliFailure, CliResult, PlakPaths, home};

pub(super) fn workspace(
    paths: &PlakPaths,
    roots: Option<Vec<PathBuf>>,
) -> CliResult<Arc<OperatorWorkspace>> {
    let excluded = paths.home().to_path_buf();
    let state = crate::home::paths::application_state_home()
        .map_err(CliFailure::new)?
        .join("plak");
    let path = crate::environment::inherited_path()
        .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    let fallback = crate::environment::login_shell()
        .zip(crate::environment::inherited_home())
        .zip(crate::environment::inherited_user())
        .map(|((shell, home), user)| OperatorAccountFallback {
            shell: shell.into(),
            home: home.into(),
            user,
        });
    Ok(Arc::new(move |candidate: &Path| {
        let canonical = candidate
            .canonicalize()
            .map_err(|_| OperatorRefusal::WorkspaceUnavailable)?;
        if canonical != candidate
            || !canonical.is_dir()
            || home::overlaps_home(&canonical, &excluded)
            || home::overlaps_home(&canonical, &state)
            || roots
                .as_ref()
                .is_some_and(|roots| !home::admitted_workspace(roots, &excluded, &canonical))
        {
            return Err(OperatorRefusal::WorkspaceUnavailable);
        }
        plakous_hosted::operator_launch(&canonical, path.clone(), fallback.clone())
    }))
}
