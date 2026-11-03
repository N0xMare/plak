//! The runner seam (unsupported, spec 01 §8.5): an execution backend whose
//! operations run on a runner reached through a [`RunnerLink`], and the
//! runner side that answers them from a local [`ExecutionEnvironment`].
//!
//! Requests and results cross the link as JSON values in a schema this module
//! owns ([`wire`]); the link frames, keys and fences them. Nothing here
//! selects a runner for a session.

mod wire;

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use plakous_model::{CancellationToken, Timestamp};
use serde_json::Value;

use super::{
    EnvironmentDelegation, EnvironmentVariableValue, ExecutionBackend, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionBackendFuture, ExecutionBackendIdentity,
    ExecutionEnvironmentLimits, ExecutionWorkspace, LineOwner, LiteralProcessResult,
    OperationClass, OperationContext, PreparedLiteralProcess, PreparedServiceProcess,
    RepositoryQuery, RepositoryQueryResult, ServiceProcess, WorkspaceMutationTransaction,
    WorkspaceMutationTransactionResult, WorkspaceReadSlice, WorkspaceReadSliceRequest,
    backend_error, ready_error, sealed,
};
use crate::{
    ExecutionBackendDescriptor, ExecutionBackendKind, ExecutionEnvironment,
    ExecutionEnvironmentConfig, ExecutionEnvironmentError, ExecutionEnvironmentErrorKind,
    ExecutionEnvironmentScope, RunnerId,
};

/// The operations a runner answers through this seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerVerb {
    /// [`ExecutionEnvironment::read_workspace`].
    ReadWorkspace,
    /// [`ExecutionEnvironment::query_repository`].
    QueryRepository,
    /// [`ExecutionEnvironment::mutate_workspace`].
    MutateWorkspace,
    /// [`ExecutionEnvironment::execute_literal_process`].
    ExecuteLiteralProcess,
}

impl RunnerVerb {
    /// Every verb.
    pub const ALL: [Self; 4] = [
        Self::ReadWorkspace,
        Self::QueryRepository,
        Self::MutateWorkspace,
        Self::ExecuteLiteralProcess,
    ];

    /// The verb's wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadWorkspace => "workspace.read",
            Self::QueryRepository => "repository.query",
            Self::MutateWorkspace => "workspace.mutate",
            Self::ExecuteLiteralProcess => "process.execute",
        }
    }

    /// The verb a wire name names, if any.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|verb| verb.as_str() == name)
    }
}

/// One operation a [`RunnerBackend`] hands its link.
#[derive(Debug)]
pub struct RunnerCall {
    verb: RunnerVerb,
    payload: Value,
    owner: Option<LineOwner>,
    class: OperationClass,
    budget: Option<Timestamp>,
    cancellation: CancellationToken,
}

impl RunnerCall {
    /// The operation.
    #[must_use]
    pub const fn verb(&self) -> RunnerVerb {
        self.verb
    }

    /// The operation's request, in this seam's schema.
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    /// The line head the operation runs for, when one started it.
    #[must_use]
    pub const fn owner(&self) -> Option<&LineOwner> {
        self.owner.as_ref()
    }

    /// The request class and key.
    #[must_use]
    pub const fn class(&self) -> OperationClass {
        self.class
    }

    /// The caller's absolute deadline, when it set one.
    #[must_use]
    pub const fn budget(&self) -> Option<Timestamp> {
        self.budget
    }

    /// The caller's cancellation: the link asks the runner to cancel once it
    /// fires.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

/// Why a link produced no result.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RunnerFailure {
    /// The operation ran, or the runner's backend refused it, with this error
    /// in this seam's schema.
    Backend(Value),
    /// The runner accepts a newer owner of the line; nothing ran.
    StaleOwner,
    /// The runner does not implement the operation; nothing ran.
    Unsupported,
    /// The link refused the call without sending it.
    Unavailable(String),
    /// The call may have run: the link was lost after it was sent, or the
    /// runner restarted under it.
    OutcomeUnknown(String),
    /// The runner refused the call as malformed or out of protocol; nothing
    /// ran.
    Refused(String),
}

/// An owned future answering one [`RunnerCall`].
pub type RunnerCallFuture = Pin<Box<dyn Future<Output = Result<Value, RunnerFailure>> + Send>>;

/// How a [`RunnerBackend`] reaches its runner. The implementation frames,
/// keys and fences each call; it never interprets the payload.
pub trait RunnerLink: Send + Sync + 'static {
    /// Sends one call and answers its result.
    fn call(&self, call: RunnerCall) -> RunnerCallFuture;
}

/// An execution backend whose operations run on a runner. Its identity is
/// always a runner descriptor naming a runner other than `local`; it is never
/// selected for a session by this crate.
pub struct RunnerBackend {
    identity: ExecutionBackendIdentity,
    link: Arc<dyn RunnerLink>,
}

impl RunnerBackend {
    /// A backend presenting the runner's `descriptor` (as its hello carried
    /// it) over `link`.
    ///
    /// # Errors
    ///
    /// Refuses a descriptor that does not decode, is not a runner descriptor,
    /// or names the reserved `local` runner.
    pub fn new(
        descriptor: Value,
        link: Arc<dyn RunnerLink>,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let invalid = || {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidIdentity,
                "runner descriptor is invalid",
            )
        };
        let descriptor: ExecutionBackendDescriptor =
            serde_json::from_value(descriptor).map_err(|_| invalid())?;
        if descriptor.kind() != ExecutionBackendKind::Runner
            || descriptor.runner_id().is_none_or(RunnerId::is_local)
        {
            return Err(invalid());
        }
        Ok(Self {
            identity: ExecutionBackendIdentity::runner(descriptor)?,
            link,
        })
    }

    fn call<T: Send + 'static>(
        &self,
        verb: RunnerVerb,
        workspace: &ExecutionWorkspace,
        request: Result<Value, ExecutionBackendError>,
        control: &OperationContext,
        decode: fn(Value) -> Result<T, ExecutionBackendError>,
    ) -> ExecutionBackendFuture<T> {
        let payload = match request.and_then(|request| wire::envelope(workspace, request)) {
            Ok(payload) => payload,
            Err(error) => return ready_error(error),
        };
        let call = RunnerCall {
            verb,
            payload,
            owner: control.owner().copied(),
            class: control.class(),
            budget: control.budget(),
            cancellation: control.cancellation().clone(),
        };
        let answer = self.link.call(call);
        Box::pin(async move {
            match answer.await {
                Ok(value) => decode(value),
                Err(failure) => Err(failure_error(failure)),
            }
        })
    }
}

fn failure_error(failure: RunnerFailure) -> ExecutionBackendError {
    match failure {
        RunnerFailure::Backend(error) => wire::decode_error(error),
        RunnerFailure::StaleOwner => ExecutionBackendError::stale_job_owner(),
        RunnerFailure::Unsupported => backend_error(
            ExecutionBackendErrorKind::Unsupported,
            "the runner does not implement this operation",
        ),
        RunnerFailure::Unavailable(detail) => ExecutionBackendError::new(
            ExecutionBackendErrorKind::Io,
            format!("the runner is unavailable: {detail}"),
        )
        .with_proven_no_effect(true),
        RunnerFailure::OutcomeUnknown(detail) => ExecutionBackendError::new(
            ExecutionBackendErrorKind::RecoveryRequired,
            format!("the runner operation's outcome is unknown: {detail}"),
        ),
        RunnerFailure::Refused(detail) => ExecutionBackendError::new(
            ExecutionBackendErrorKind::InvalidRequest,
            format!("the runner refused the operation: {detail}"),
        ),
    }
}

impl sealed::Sealed for RunnerBackend {}

impl ExecutionBackend for RunnerBackend {
    fn identity(&self) -> &ExecutionBackendIdentity {
        &self.identity
    }

    /// The runner resolves its own delegations from its own environment.
    fn resolve_environment(
        &self,
        _binding: &EnvironmentDelegation,
    ) -> Option<EnvironmentVariableValue> {
        None
    }

    fn read_workspace(
        &self,
        workspace: ExecutionWorkspace,
        request: WorkspaceReadSliceRequest,
        control: OperationContext,
    ) -> ExecutionBackendFuture<WorkspaceReadSlice> {
        self.call(
            RunnerVerb::ReadWorkspace,
            &workspace,
            wire::encode_read_request(&request),
            &control,
            wire::decode_read_slice,
        )
    }

    fn query_repository(
        &self,
        workspace: ExecutionWorkspace,
        query: RepositoryQuery,
        control: OperationContext,
    ) -> ExecutionBackendFuture<RepositoryQueryResult> {
        self.call(
            RunnerVerb::QueryRepository,
            &workspace,
            wire::encode_query(&query),
            &control,
            wire::decode_query_result,
        )
    }

    fn mutate_workspace(
        &self,
        workspace: ExecutionWorkspace,
        transaction: WorkspaceMutationTransaction,
        control: OperationContext,
    ) -> ExecutionBackendFuture<WorkspaceMutationTransactionResult> {
        self.call(
            RunnerVerb::MutateWorkspace,
            &workspace,
            wire::encode_transaction(&transaction),
            &control,
            wire::decode_transaction_result,
        )
    }

    /// The runner prepares the request with its own delegations and applies
    /// its own environment's limits.
    fn execute_literal_process(
        &self,
        workspace: ExecutionWorkspace,
        request: PreparedLiteralProcess,
        _limits: ExecutionEnvironmentLimits,
        control: OperationContext,
    ) -> ExecutionBackendFuture<LiteralProcessResult> {
        self.call(
            RunnerVerb::ExecuteLiteralProcess,
            &workspace,
            wire::encode_process_request(request.request()),
            &control,
            wire::decode_process_result,
        )
    }

    /// Services and their streams are not carried by this seam yet.
    fn spawn_service(
        &self,
        _workspace: ExecutionWorkspace,
        _request: PreparedServiceProcess,
        _control: OperationContext,
    ) -> ExecutionBackendFuture<ServiceProcess> {
        ready_error(backend_error(
            ExecutionBackendErrorKind::Unsupported,
            "the runner does not carry services yet",
        ))
    }
}

/// An environment whose operations run through `backend`, over the workspace
/// `root` and `cwd` as they are named on the runner.
///
/// # Errors
///
/// Returns a workspace error, as [`ExecutionEnvironment::host`] does.
pub fn runner_environment(
    scope: ExecutionEnvironmentScope,
    root: impl AsRef<Path>,
    cwd: impl AsRef<Path>,
    config: ExecutionEnvironmentConfig,
    backend: Arc<RunnerBackend>,
) -> Result<ExecutionEnvironment, ExecutionEnvironmentError> {
    let backend: Arc<dyn ExecutionBackend> = backend;
    ExecutionEnvironment::with_operation_backend(scope, root, cwd, config, backend)
}

/// The descriptor a runner presents for `environment`'s backend under the
/// runner identity `runner`, as its hello carries it.
///
/// # Errors
///
/// Refuses the reserved `local` runner id.
pub fn runner_descriptor(
    environment: &ExecutionEnvironment,
    runner: RunnerId,
) -> Result<Value, ExecutionEnvironmentError> {
    if runner.is_local() {
        return Err(ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::InvalidIdentity,
            "a runner presents a runner id other than local",
        ));
    }
    let mut descriptor = environment.descriptor().backend().clone();
    descriptor.runner_id = Some(runner);
    serde_json::to_value(&descriptor).map_err(|_| {
        ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::InvalidIdentity,
            "runner descriptor could not be encoded",
        )
    })
}

/// Why the runner side answered no result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServeFailure {
    /// The verb is not one this seam answers; nothing ran.
    Unsupported,
    /// The operation ran, or was refused, with this error in this seam's
    /// schema.
    Backend {
        /// The error.
        error: Value,
        /// Whether the operation provably had no effect.
        proven_no_effect: bool,
    },
}

/// An owned future answering one served operation.
pub type ServeFuture = Pin<Box<dyn Future<Output = Result<Value, ServeFailure>> + Send>>;

/// Answers one operation from `environment`, as the runner side of a link.
/// The request must name `environment`'s own workspace.
pub fn serve(
    environment: &ExecutionEnvironment,
    verb: &str,
    payload: Value,
    control: OperationContext,
) -> ServeFuture {
    let Some(verb) = RunnerVerb::parse(verb) else {
        return Box::pin(std::future::ready(Err(ServeFailure::Unsupported)));
    };
    let request = match wire::open_envelope(environment, payload) {
        Ok(request) => request,
        Err(error) => return Box::pin(std::future::ready(Err(backend_failure(&error)))),
    };
    let environment = environment.clone();
    Box::pin(async move {
        let answer = match verb {
            RunnerVerb::ReadWorkspace => match wire::decode_read_request(request) {
                Ok(request) => environment
                    .read_workspace(request, control)
                    .await
                    .and_then(|slice| wire::encode_read_slice(&slice)),
                Err(error) => Err(error),
            },
            RunnerVerb::QueryRepository => match wire::decode_query(request) {
                Ok(query) => environment
                    .query_repository(query, control)
                    .await
                    .and_then(|result| wire::encode_query_result(&result)),
                Err(error) => Err(error),
            },
            RunnerVerb::MutateWorkspace => match wire::decode_transaction(request) {
                Ok(transaction) => environment
                    .mutate_workspace(transaction, control)
                    .await
                    .and_then(|result| wire::encode_transaction_result(&result)),
                Err(error) => Err(error),
            },
            RunnerVerb::ExecuteLiteralProcess => match wire::decode_process_request(request) {
                Ok(request) => environment
                    .execute_literal_process(request, control)
                    .await
                    .and_then(|result| wire::encode_process_result(&result)),
                Err(error) => Err(error),
            },
        };
        answer.map_err(|error| backend_failure(&error))
    })
}

/// The error a runner answers in place of a result too large for one frame.
/// The operation ran, so its outcome is not proven to have had no effect.
#[must_use]
pub fn answer_too_large() -> Value {
    wire::encode_error(
        &backend_error(
            ExecutionBackendErrorKind::ResourceLimit,
            "the runner's answer exceeds its frame limit",
        )
        .with_proven_no_effect(false),
    )
}

fn backend_failure(error: &ExecutionBackendError) -> ServeFailure {
    ServeFailure::Backend {
        error: wire::encode_error(error),
        proven_no_effect: error.proven_no_effect(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
