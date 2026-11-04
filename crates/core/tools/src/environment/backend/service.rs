//! Long-lived service processes: piped stdio, a bounded stderr tail, and an
//! owned process group that termination joins.

use std::collections::VecDeque;
use std::fmt;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::Zeroizing;

#[cfg(unix)]
mod host;
#[cfg(unix)]
use host::HostService;
#[cfg(unix)]
pub(super) use host::spawn_host_service;

use super::deterministic::FakeProcessGuard;
use super::{
    EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendErrorKind, MAX_EXECUTION_VALUE_BYTES, MAX_LITERAL_PROCESS_ARGUMENTS,
    backend_error, invalid_request, lock,
};

/// Maximum bytes of a service process's stderr retained for diagnostics.
pub const MAX_SERVICE_STDERR_TAIL_BYTES: usize = 8 * 1024;
/// Maximum declared plus secret environment entries of one service process.
pub const MAX_SERVICE_ENVIRONMENT_ENTRIES: usize = 256;
/// Grace between SIGTERM and SIGKILL when a service process is terminated.
pub const SERVICE_PROCESS_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// One secret environment value for a service process.
///
/// Not cloneable, not serializable, redacted in `Debug`, and zeroized on drop.
pub struct ServiceSecretValue(
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "read only by the unix host backend")
    )]
    Zeroizing<String>,
);

impl ServiceSecretValue {
    /// Takes ownership of one resolved secret value.
    ///
    /// # Errors
    ///
    /// Rejects an empty, oversized, or NUL-bearing value.
    pub fn new(value: String) -> Result<Self, ExecutionBackendError> {
        let value = Zeroizing::new(value);
        if value.is_empty() || value.len() > MAX_EXECUTION_VALUE_BYTES || value.contains('\0') {
            return Err(invalid_request("service process secret value is invalid"));
        }
        Ok(Self(value))
    }

    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "read only by the unix host backend")
    )]
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ServiceSecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ServiceSecretValue(<redacted>)")
    }
}

/// A validated long-lived service process request.
///
/// Built by the embedding host from frozen configuration; no tool input
/// deserializes into it, so a model never supplies its argv or environment.
pub struct ServiceProcessRequest {
    program: Arc<str>,
    arguments: Arc<[String]>,
    environment: Vec<(EnvironmentVariableName, EnvironmentVariableValue)>,
    secret_environment: Vec<(EnvironmentVariableName, ServiceSecretValue)>,
    /// Secrets the runner that spawns the service resolves from its own
    /// store, by variable and secret name: no value is on the host.
    runner_secrets: Vec<(EnvironmentVariableName, String)>,
    stderr_tail_bytes: usize,
}

impl ServiceProcessRequest {
    /// Creates a bounded service request.
    ///
    /// `program` is absolute or a basename looked up on the child's PATH.
    /// `environment` holds declared literal values; `secret_environment`
    /// holds resolved secrets. A name may appear once across both.
    ///
    /// # Errors
    ///
    /// Rejects an empty or invalid program, a relative program path,
    /// invalid or too many arguments or environment entries, a duplicate
    /// environment name, and a stderr tail outside
    /// `1..=MAX_SERVICE_STDERR_TAIL_BYTES`.
    pub fn new(
        program: impl Into<Arc<str>>,
        arguments: impl IntoIterator<Item = String>,
        environment: impl IntoIterator<Item = (EnvironmentVariableName, String)>,
        secret_environment: impl IntoIterator<Item = (EnvironmentVariableName, ServiceSecretValue)>,
        stderr_tail_bytes: usize,
    ) -> Result<Self, ExecutionBackendError> {
        let program = program.into();
        if program.is_empty()
            || program.len() > MAX_EXECUTION_VALUE_BYTES
            || program.contains('\0')
            || (!Path::new(&*program).is_absolute() && program.contains('/'))
        {
            return Err(invalid_request(
                "service process program must be absolute or a PATH basename",
            ));
        }
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        if arguments.len() > MAX_LITERAL_PROCESS_ARGUMENTS {
            return Err(invalid_request("service process has too many arguments"));
        }
        let mut argument_bytes = 0_usize;
        for argument in &arguments {
            if argument.contains('\0') {
                return Err(invalid_request("service process argument is invalid"));
            }
            argument_bytes = argument_bytes.saturating_add(argument.len());
        }
        if argument_bytes > MAX_EXECUTION_VALUE_BYTES {
            return Err(invalid_request(
                "service process arguments exceed their byte limit",
            ));
        }
        let environment = environment
            .into_iter()
            .map(|(name, value)| {
                if value.len() > MAX_EXECUTION_VALUE_BYTES || value.contains('\0') {
                    Err(invalid_request(
                        "service process environment value is invalid",
                    ))
                } else {
                    Ok((name, EnvironmentVariableValue(value.into())))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let secret_environment = secret_environment.into_iter().collect::<Vec<_>>();
        if environment.len() + secret_environment.len() > MAX_SERVICE_ENVIRONMENT_ENTRIES {
            return Err(invalid_request(
                "service process has too many environment entries",
            ));
        }
        let mut names = environment
            .iter()
            .map(|(name, _)| name)
            .chain(secret_environment.iter().map(|(name, _)| name))
            .collect::<Vec<_>>();
        names.sort();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid_request(
                "service process environment names a variable twice",
            ));
        }
        if stderr_tail_bytes == 0 || stderr_tail_bytes > MAX_SERVICE_STDERR_TAIL_BYTES {
            return Err(invalid_request(
                "service process stderr tail is outside its bound",
            ));
        }
        Ok(Self {
            program,
            arguments: arguments.into(),
            environment,
            secret_environment,
            runner_secrets: Vec::new(),
            stderr_tail_bytes,
        })
    }

    /// The same request naming `secrets` (variable, secret name) for the
    /// spawning runner to resolve from its own store.
    pub(crate) fn with_runner_secrets(
        mut self,
        secrets: Vec<(EnvironmentVariableName, String)>,
    ) -> Result<Self, ExecutionBackendError> {
        if self.environment.len() + self.secret_environment.len() + secrets.len()
            > MAX_SERVICE_ENVIRONMENT_ENTRIES
        {
            return Err(invalid_request(
                "service process has too many environment entries",
            ));
        }
        if secrets.iter().any(|(_, secret)| {
            secret.is_empty() || secret.len() > MAX_EXECUTION_VALUE_BYTES || secret.contains('\0')
        }) {
            return Err(invalid_request(
                "service process runner secret name is invalid",
            ));
        }
        let mut names = self
            .environment
            .iter()
            .map(|(name, _)| name)
            .chain(self.secret_environment.iter().map(|(name, _)| name))
            .chain(secrets.iter().map(|(name, _)| name))
            .collect::<Vec<_>>();
        names.sort();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid_request(
                "service process environment names a variable twice",
            ));
        }
        self.runner_secrets = secrets;
        Ok(self)
    }

    /// The secrets the spawning runner resolves, by variable and name.
    pub(crate) fn runner_secrets(&self) -> &[(EnvironmentVariableName, String)] {
        &self.runner_secrets
    }

    /// Returns the exact program text.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Returns exact, separately retained arguments.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Returns declared environment names without values.
    pub fn environment_names(&self) -> impl Iterator<Item = &EnvironmentVariableName> {
        self.environment.iter().map(|(name, _)| name)
    }

    /// The declared environment with its values, for a runner request.
    pub(super) fn environment_entries(
        &self,
    ) -> impl Iterator<Item = (&EnvironmentVariableName, &str)> {
        self.environment
            .iter()
            .map(|(name, value)| (name, &*value.0))
    }

    /// Returns secret environment names without values.
    pub fn secret_environment_names(&self) -> impl Iterator<Item = &EnvironmentVariableName> {
        self.secret_environment.iter().map(|(name, _)| name)
    }

    /// Returns the retained stderr tail bound in bytes.
    #[must_use]
    pub const fn stderr_tail_bytes(&self) -> usize {
        self.stderr_tail_bytes
    }
}

impl fmt::Debug for ServiceProcessRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceProcessRequest")
            .field("program_bytes", &self.program.len())
            .field("argument_count", &self.arguments.len())
            .field("environment_count", &self.environment.len())
            .field("secret_environment_count", &self.secret_environment.len())
            .field("runner_secret_count", &self.runner_secrets.len())
            .field("stderr_tail_bytes", &self.stderr_tail_bytes)
            .finish()
    }
}

/// A service request after the environment's delegation allowlist resolved.
///
/// Only [`crate::ExecutionEnvironment::spawn_service`] builds one.
pub struct PreparedServiceProcess {
    pub(super) request: ServiceProcessRequest,
    pub(super) inherited: Arc<[(EnvironmentVariableName, EnvironmentVariableValue)]>,
}

impl PreparedServiceProcess {
    /// Returns the original validated request.
    #[must_use]
    pub const fn request(&self) -> &ServiceProcessRequest {
        &self.request
    }

    /// The child's non-secret environment: inherited delegations, then
    /// declared values replacing any inherited value of the same name.
    pub(super) fn child_environment(
        &self,
    ) -> Vec<(EnvironmentVariableName, EnvironmentVariableValue)> {
        let mut environment = self
            .inherited
            .iter()
            .filter(|(name, _)| {
                !self
                    .request
                    .environment
                    .iter()
                    .any(|(declared, _)| declared == name)
            })
            .cloned()
            .collect::<Vec<_>>();
        environment.extend(self.request.environment.iter().cloned());
        environment
    }
}

impl fmt::Debug for PreparedServiceProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedServiceProcess")
            .field("request", &self.request)
            .field("inherited_count", &self.inherited.len())
            .finish()
    }
}

/// A running long-lived service process.
///
/// The stdio halves are the service's protocol channel. Dropping the
/// handle without [`ServiceHandle::terminate`] kills the process group
/// without joining it.
#[non_exhaustive]
pub struct ServiceProcess {
    /// Writes to the service's stdin.
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    /// Reads the service's stdout.
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    /// Lifetime authority and stderr diagnostics.
    pub handle: ServiceHandle,
}

impl ServiceProcess {
    pub(super) fn new(
        stdin: Pin<Box<dyn AsyncWrite + Send>>,
        stdout: Pin<Box<dyn AsyncRead + Send>>,
        handle: ServiceHandle,
    ) -> Self {
        Self {
            stdin,
            stdout,
            handle,
        }
    }
}

impl fmt::Debug for ServiceProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceProcess")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

/// Bounded ring of the most recent stderr bytes.
pub(super) struct ServiceStderrTail {
    capacity: usize,
    bytes: Mutex<VecDeque<u8>>,
}

impl ServiceStderrTail {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            bytes: Mutex::new(VecDeque::with_capacity(capacity)),
        }
    }

    pub(super) fn push(&self, chunk: &[u8]) {
        let chunk = &chunk[chunk.len().saturating_sub(self.capacity)..];
        let mut bytes = lock(&self.bytes);
        let overflow = (bytes.len() + chunk.len()).saturating_sub(self.capacity);
        bytes.drain(..overflow);
        bytes.extend(chunk);
    }

    fn sanitized(&self) -> String {
        let bytes = lock(&self.bytes).iter().copied().collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes)
            .chars()
            .map(|ch| {
                if ch.is_control() && ch != '\n' && ch != '\t' {
                    '\u{fffd}'
                } else {
                    ch
                }
            })
            .collect()
    }
}

/// Lifetime authority over one service process.
pub struct ServiceHandle {
    owner: ServiceOwner,
    stderr_tail: Arc<ServiceStderrTail>,
}

pub(super) enum ServiceOwner {
    #[cfg(unix)]
    Host(HostService),
    /// A service on a runner, terminated through its link.
    Runner(super::runner::RunnerServiceOwner),
    /// A deterministic fake's live-resource guard; dropping it releases it.
    Fake(FakeProcessGuard),
}

impl ServiceHandle {
    pub(super) fn new(owner: ServiceOwner, stderr_tail: Arc<ServiceStderrTail>) -> Self {
        Self { owner, stderr_tail }
    }

    /// Returns the retained stderr tail for bind and crash diagnostics.
    ///
    /// Lossy UTF-8 with control characters other than newline and tab
    /// replaced; at most the request's tail bound. It is diagnostic text
    /// for the operator, not model-visible output.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.sanitized()
    }

    /// Terminates the service and joins its process group.
    ///
    /// Sends SIGTERM to the owned process group, waits up to `grace`, then
    /// sends SIGKILL and waits a further bounded join. A descendant that left
    /// the group also left this cleanup authority.
    ///
    /// # Errors
    ///
    /// Returns `CleanupUnresolved` when the group could not be proven joined
    /// within the bound, or an I/O error when the child cannot be waited.
    pub async fn terminate(self, grace: Duration) -> Result<(), ExecutionBackendError> {
        match self.owner {
            #[cfg(unix)]
            ServiceOwner::Host(host) => host.terminate(grace).await,
            ServiceOwner::Runner(runner) => runner.terminate(grace, &self.stderr_tail).await,
            ServiceOwner::Fake(guard) => {
                drop(guard);
                Ok(())
            }
        }
    }
}

impl fmt::Debug for ServiceHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let owner = match &self.owner {
            #[cfg(unix)]
            ServiceOwner::Host(_) => "host",
            ServiceOwner::Runner(_) => "runner",
            ServiceOwner::Fake(_) => "deterministic",
        };
        formatter
            .debug_struct("ServiceHandle")
            .field("owner", &owner)
            .field("stderr_tail", &"<redacted>")
            .finish()
    }
}

pub(super) fn cancelled_after_spawn() -> ExecutionBackendError {
    backend_error(
        ExecutionBackendErrorKind::Cancelled,
        "service process startup was cancelled and its process group joined",
    )
}

#[cfg(test)]
mod tests;
