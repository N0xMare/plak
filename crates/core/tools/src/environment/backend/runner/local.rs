//! The home-local runner as a transport for the local runner's processes
//! (spec 01 §8.4, §8.5): an environment whose descriptor, run-stable facts,
//! files and version control are the in-process local backend's, and whose
//! process sessions run on the home's own runner daemon, so they outlive the
//! host process that started them.

use std::sync::Arc;

use super::{RunnerLink, RunnerSessions};
use crate::{
    ExecutionBackendError, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentScope,
    HostProcessOptions, JoinedProcessSessions, WorkspacePolicy,
};

/// The home-local runner an environment's process sessions run on, and the
/// runtime its link runs on.
#[derive(Clone)]
pub(in crate::environment) struct LocalDaemon {
    link: Arc<dyn RunnerLink>,
    runtime: tokio::runtime::Handle,
}

/// A local-runner environment, as [`ExecutionEnvironment::host_with_policy_and_process_options`]
/// builds it, whose process sessions run on the home-local runner behind
/// `link` (on `runtime`). Its descriptor and run-stable facts are the
/// local runner's: the daemon is a transport, not a runner of its own.
/// Each process start carries the `PATH`, `HOME` and discovery ceiling this
/// environment resolves, and nothing of the host's own environment.
/// Unsupported: only tests select it (spec 01 §8.5).
///
/// # Errors
///
/// Refuses a configuration that delegates an environment variable or a
/// credential, whose value would have to cross the link, and anything the
/// local constructor refuses.
pub fn local_daemon_environment(
    scope: ExecutionEnvironmentScope,
    workspace: WorkspacePolicy,
    config: ExecutionEnvironmentConfig,
    options: HostProcessOptions,
    link: Arc<dyn RunnerLink>,
    runtime: tokio::runtime::Handle,
) -> Result<ExecutionEnvironment, ExecutionEnvironmentError> {
    if !config.environment_delegations().is_empty() || !config.credential_delegations().is_empty() {
        return Err(ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::InvalidDelegationSourceAuthority,
            "a home-local runner carries no delegated value",
        ));
    }
    let mut environment = ExecutionEnvironment::host_with_policy_and_process_options(
        scope, workspace, config, options,
    )?;
    environment.local_daemon = Some(LocalDaemon { link, runtime });
    Ok(environment)
}

impl ExecutionEnvironment {
    /// The process-session service of an environment on the home-local
    /// runner: its line's sessions there, started in this environment's own
    /// process context. Without a line owner it refuses every start.
    pub(in crate::environment) fn local_daemon_sessions(
        &self,
        daemon: &LocalDaemon,
    ) -> Result<JoinedProcessSessions, ExecutionBackendError> {
        let (Some(owner), Some(process)) = (self.line_owner, self.host_process.clone()) else {
            return JoinedProcessSessions::refusing(self);
        };
        let sessions = RunnerSessions::from_link(Arc::clone(&daemon.link), self, owner)
            .with_local_process(process);
        JoinedProcessSessions::runner(self, sessions, &daemon.runtime)
    }
}
