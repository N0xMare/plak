//! Host workspace policy resolution; core tools only enforces the supplied facts.

use crate::{CliFailure, CliResult};
use plakous::tools::HostProcessOptions;
use std::path::{Path, PathBuf};

pub(crate) fn options(workspace: &Path) -> CliResult<HostProcessOptions> {
    let relative = PathBuf::from(".plak/bin");
    let directory = workspace.join(&relative);
    let shim_present = directory.join("git").is_file() || directory.join("jj").is_file();
    // Tool tier does not identify a copy: read-only children inherit their
    // parent's workspace, including an ordinary checkout without a shim.
    let known_copy = crate::git_redirect_fence::is_minted_occupancy_copy(workspace);
    if known_copy && !shim_present {
        return Err(CliFailure::new(
            "required workspace process shim is unavailable",
        ));
    }
    let required = (known_copy || shim_present).then_some(relative);
    HostProcessOptions::new(required)
        .map(|options| {
            options.with_discovery_ceiling(crate::git_redirect_fence::is_minted_jj_copy(workspace))
        })
        .map_err(|_| CliFailure::new("host process prefix policy is invalid"))
}

#[cfg(test)]
mod tests {
    use super::options;

    #[test]
    fn inherited_workspace_does_not_require_minted_copy_shim() {
        let parent = tempfile::tempdir().expect("inherited workspace");
        assert!(options(parent.path()).is_ok());
    }

    #[test]
    fn minted_workspace_shim_cannot_disappear_before_construction() {
        let directory = tempfile::tempdir().expect("workspace");
        for kind in ["worktrees", "workspaces"] {
            let minted = directory.path().join(".plak").join(kind).join("copy");
            std::fs::create_dir_all(&minted).expect("copy");
            assert!(options(&minted).is_err());
            let prefix = minted.join(".plak/bin");
            std::fs::create_dir_all(&prefix).expect("prefix");
            assert!(options(&minted).is_err());
            for shim in ["git", "jj"] {
                std::fs::write(prefix.join(shim), "fixture").expect("shim");
                assert!(options(&minted).is_ok());
                std::fs::remove_file(prefix.join(shim)).expect("remove shim");
                assert!(options(&minted).is_err());
            }
        }
    }
    #[test]
    fn installed_shim_requirement_survives_durable_reconstruction() {
        use plakous::tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentDescriptor,
            ExecutionEnvironmentScope, WorkspacePolicy,
        };
        use std::str::FromStr;

        let directory = tempfile::tempdir().expect("workspace");
        for relative in ["ordinary", ".plak/worktrees/copy", ".plak/workspaces/copy"] {
            let workspace = directory.path().join(relative);
            let prefix = workspace.join(".plak/bin");
            std::fs::create_dir_all(&prefix).expect("prefix");
            std::fs::write(prefix.join("git"), "fixture").expect("shim");
            let scope = ExecutionEnvironmentScope::Run(
                plakous::model::RunId::from_str("00000000-0000-0000-0000-000000000001")
                    .expect("run id"),
            );
            let environment = ExecutionEnvironment::host_with_policy_and_process_options(
                scope,
                WorkspacePolicy::host(&workspace, ".").expect("workspace policy"),
                ExecutionEnvironmentConfig::default(),
                options(&workspace).expect("host options"),
            )
            .expect("host environment");
            let serialized = serde_json::to_vec(environment.descriptor()).expect("serialize");
            let restored: ExecutionEnvironmentDescriptor =
                serde_json::from_slice(&serialized).expect("restore descriptor");
            assert!(
                ExecutionEnvironment::host(
                    restored.scope(),
                    &workspace,
                    ".",
                    restored.config().clone(),
                )
                .is_ok()
            );
            std::fs::remove_dir_all(&prefix).expect("remove prefix");
            assert!(
                ExecutionEnvironment::host(
                    restored.scope(),
                    &workspace,
                    ".",
                    restored.config().clone(),
                )
                .is_err()
            );
        }
    }
}
