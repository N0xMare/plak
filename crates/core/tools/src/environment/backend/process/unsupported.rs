//! The host process surface on a target without host processes.
//!
//! No host environment exists here: [`HostProcessContext::capture`] refuses,
//! so every item that takes a context or a joined input is unreachable.

use std::sync::Arc;

use super::super::{ExecutionBackend, ExecutionEnvironmentConfig};
use super::{
    Duration, EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits,
    ExecutionWorkspace, Instant, JoinedProcessStartRequest, JoinedSession, OperationContext,
    PendingJobOutput,
};

pub(in crate::environment) enum HostProcessContext {}

impl HostProcessContext {
    pub(in crate::environment) fn capture(
        _workspace: &crate::WorkspacePolicy,
        _options: super::super::super::HostProcessOptions,
    ) -> Result<Self, ExecutionEnvironmentError> {
        Err(ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::Workspace,
            "host processes need a unix target",
        ))
    }

    pub(in crate::environment) fn facts(
        &self,
    ) -> Result<super::ProcessFacts, ExecutionBackendError> {
        match *self {}
    }

    pub(in crate::environment) fn from_facts(
        _facts: super::ProcessFacts,
    ) -> Result<Self, ExecutionBackendError> {
        Err(ExecutionBackendError::new(
            super::ExecutionBackendErrorKind::Unsupported,
            "host processes need a unix target",
        ))
    }
}

pub(in crate::environment) fn host_execution_backend(
    _config: &ExecutionEnvironmentConfig,
    process: &Arc<HostProcessContext>,
) -> Result<Arc<dyn ExecutionBackend>, ExecutionEnvironmentError> {
    match **process {}
}

pub(super) enum JoinedSessionInput {}

pub(super) enum HostProcessOwner {}

impl HostProcessOwner {
    pub(super) fn request_stop(&self) {
        match *self {}
    }

    pub(super) fn join_until(&mut self, _deadline: Instant) -> Result<bool, ExecutionBackendError> {
        match *self {}
    }
}

pub(super) fn start_joined_host_process(
    _workspace: &ExecutionWorkspace,
    process: &HostProcessContext,
    _environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
    _limits: ExecutionEnvironmentLimits,
    _queue_limit: usize,
    _request: &JoinedProcessStartRequest,
    _control: OperationContext,
    _pending_spool: Option<PendingJobOutput>,
) -> Result<JoinedSession, ExecutionBackendError> {
    match *process {}
}

pub(super) fn write_joined_session_input(
    input: &mut JoinedSessionInput,
    _bytes: &[u8],
    _duration: Duration,
) -> Result<(), ExecutionBackendError> {
    match *input {}
}

pub(super) fn close_joined_session_input(
    input: &mut JoinedSessionInput,
    _duration: Duration,
) -> Result<(), ExecutionBackendError> {
    match *input {}
}
