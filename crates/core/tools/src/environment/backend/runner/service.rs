//! Services on a runner: the host end of a spawned service, whose stdio is
//! one stream of the link, and the runner side that spawns and terminates
//! it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::service::{ServiceOwner, ServiceStderrTail};
use super::super::{
    ExecutionBackendError, ExecutionBackendErrorKind, ExecutionBackendFuture, ExecutionWorkspace,
    OperationContext, ServiceHandle, ServiceProcess, ServiceProcessRequest, backend_error,
    ready_error,
};
use super::wire::{self, from_value, to_value};
use super::{
    RunnerCall, RunnerClass, RunnerLink, RunnerVerb, ServeFailure, ServeFuture, backend_failure,
    failure_error,
};
use crate::{EnvironmentVariableName, ExecutionEnvironment};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ServiceRequestWire {
    program: String,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    stderr_tail_bytes: usize,
}

/// One spawned service, as the runner names it: the id of the stream that
/// carries its stdio, which the link reads from this field.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ServiceWire {
    stream: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TerminateWire {
    stream: u64,
    grace_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TerminatedWire {
    stderr_tail: String,
}

/// The host end of a service on a runner: terminating it terminates the
/// runner's process group through the link.
pub(in crate::environment::backend) struct RunnerServiceOwner {
    link: Option<Arc<dyn RunnerLink>>,
    service: u64,
}

impl RunnerServiceOwner {
    pub(in crate::environment::backend) async fn terminate(
        mut self,
        grace: Duration,
        tail: &ServiceStderrTail,
    ) -> Result<(), ExecutionBackendError> {
        let Some(link) = self.link.take() else {
            return Ok(());
        };
        let answer = link.call(terminate_call(self.service, grace)?).await;
        let terminated: TerminatedWire = from_value(answer.map_err(failure_error)?)?;
        tail.push(terminated.stderr_tail.as_bytes());
        Ok(())
    }
}

impl Drop for RunnerServiceOwner {
    /// Dropping a service unterminated kills it, as a host service is
    /// killed: the runner terminates it with no grace.
    fn drop(&mut self) {
        let Some(link) = self.link.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if let Ok(call) = terminate_call(self.service, Duration::ZERO) {
            let answer = link.call(call);
            runtime.spawn(async move {
                let _ = answer.await;
            });
        }
    }
}

fn terminate_call(service: u64, grace: Duration) -> Result<RunnerCall, ExecutionBackendError> {
    let payload = to_value(&TerminateWire {
        stream: service,
        grace_ms: u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
    })?;
    Ok(RunnerCall::new(
        RunnerVerb::ServiceTerminate,
        payload,
        &OperationContext::new(plakous_model::CancellationToken::new()),
    ))
}

/// Spawns a service on the runner behind `link`. Secret environment values
/// stay on the host: a request that names one is refused with no effect.
pub(super) fn spawn(
    link: &Arc<dyn RunnerLink>,
    workspace: &ExecutionWorkspace,
    request: &ServiceProcessRequest,
    control: &OperationContext,
) -> ExecutionBackendFuture<ServiceProcess> {
    if request.secret_environment_names().next().is_some() {
        return ready_error(backend_error(
            ExecutionBackendErrorKind::Unsupported,
            "a runner service does not carry secret environment values",
        ));
    }
    let payload = to_value(&ServiceRequestWire {
        program: request.program().to_owned(),
        arguments: request.arguments().to_vec(),
        environment: request
            .environment_entries()
            .map(|(name, value)| (name.as_str().to_owned(), value.to_owned()))
            .collect(),
        stderr_tail_bytes: request.stderr_tail_bytes(),
    })
    .and_then(|request| wire::envelope(workspace, request));
    let payload = match payload {
        Ok(payload) => payload,
        Err(error) => return ready_error(error),
    };
    let tail_bytes = request.stderr_tail_bytes();
    let link = Arc::clone(link);
    let opened = link.open(
        RunnerCall::new(RunnerVerb::ServiceSpawn, payload, control)
            .with_class(RunnerClass::Unkeyed),
    );
    Box::pin(async move {
        let (value, streams) = opened.await.map_err(failure_error)?;
        let spawned: ServiceWire = from_value(value)?;
        let owner = RunnerServiceOwner {
            link: Some(link),
            service: spawned.stream,
        };
        Ok(ServiceProcess::new(
            streams.input,
            streams.output,
            ServiceHandle::new(
                ServiceOwner::Runner(owner),
                Arc::new(ServiceStderrTail::new(tail_bytes)),
            ),
        ))
    })
}

/// An owned future spawning one service on the runner side.
pub type ServeServiceFuture =
    Pin<Box<dyn Future<Output = Result<ServiceProcess, ServeFailure>> + Send>>;

/// Spawns the service `payload` asks for from `environment`, as the runner
/// side of a link; the caller carries its stdio as stream `service` and
/// answers [`service_spawned`].
pub fn serve_service_spawn(
    environment: &ExecutionEnvironment,
    payload: Value,
    control: OperationContext,
) -> ServeServiceFuture {
    let request = wire::open_envelope(environment, payload)
        .and_then(from_value::<ServiceRequestWire>)
        .and_then(|wire| {
            let environment = wire
                .environment
                .into_iter()
                .map(|(name, value)| {
                    EnvironmentVariableName::new(name)
                        .map(|name| (name, value))
                        .map_err(|_| wire::malformed())
                })
                .collect::<Result<Vec<_>, _>>()?;
            ServiceProcessRequest::new(
                wire.program,
                wire.arguments,
                environment,
                Vec::new(),
                wire.stderr_tail_bytes,
            )
        });
    let request = match request {
        Ok(request) => request,
        Err(error) => return Box::pin(std::future::ready(Err(backend_failure(&error)))),
    };
    let spawned = environment.spawn_service(request, control);
    Box::pin(async move { spawned.await.map_err(|error| backend_failure(&error)) })
}

/// The answer to a spawn whose stdio is stream `service`.
#[must_use]
pub fn service_spawned(service: u64) -> Value {
    json!({ "stream": service })
}

/// The service a terminate request names, when it is well formed.
#[must_use]
pub fn terminated_service(payload: &Value) -> Option<u64> {
    from_value::<TerminateWire>(payload.clone())
        .ok()
        .map(|wire| wire.stream)
}

/// Terminates `handle` as `payload` asks, as the runner side of a link, and
/// answers with its stderr tail. Without a handle the service is not
/// running there: that is a refusal with no effect.
pub fn serve_service_terminate(handle: Option<ServiceHandle>, payload: Value) -> ServeFuture {
    let grace = match from_value::<TerminateWire>(payload) {
        Ok(wire) => Duration::from_millis(wire.grace_ms),
        Err(error) => return Box::pin(std::future::ready(Err(backend_failure(&error)))),
    };
    let Some(handle) = handle else {
        let error = backend_error(
            ExecutionBackendErrorKind::NotFound,
            "the service is not running on this runner",
        );
        return Box::pin(std::future::ready(Err(backend_failure(&error))));
    };
    Box::pin(async move {
        let tail = handle.stderr_tail();
        handle
            .terminate(grace)
            .await
            .map_err(|error| backend_failure(&error))?;
        to_value(&TerminatedWire { stderr_tail: tail }).map_err(|error| backend_failure(&error))
    })
}
