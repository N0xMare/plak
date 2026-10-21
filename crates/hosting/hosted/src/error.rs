//! Product CLI failure taxonomy shared between the host runtime and Plak application.

/// Terminal outcome for CLI workflows that may be interrupted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOutcome {
    /// Workflow completed successfully.
    Success,
    /// Workflow was interrupted before completion.
    Interrupted,
    /// The non-interactive run reached an operator bound.
    Bounded,
}

/// Failure category for CLI exit-code mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliFailureKind {
    /// Internal product failure.
    Internal,
    /// Usage / argument error.
    Usage,
    /// Authentication failure.
    Auth,
    /// Agent/runtime failure.
    Agent,
    /// Explicit operator recovery is required; no silent bootstrap.
    RecoveryRequired,
}

impl CliFailureKind {
    /// Exit code for this failure category.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Internal => 1,
            Self::Usage => 2,
            Self::Auth => 3,
            Self::Agent | Self::RecoveryRequired => 5,
        }
    }
}

/// Structured CLI failure with an exit-code category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliFailure {
    kind: CliFailureKind,
    message: String,
    shutdown: Option<Box<ShutdownFailures>>,
    source: Option<Box<LookupFailure>>,
    refusal: Option<StoreRefusal>,
}

/// A semantic- or product-store refusal that the same call may clear when
/// retried: nothing was written and no authority was lost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreRefusal {
    /// Another connection held the store, or its request queue was full.
    Busy,
    /// The store could not be reached or was shutting down.
    Unavailable,
}

impl StoreRefusal {
    fn of(error: &plakous::sessions::AgentSessionsError) -> Option<Self> {
        match error.kind() {
            plakous::sessions::AgentSessionsErrorKind::StorageBusy => Some(Self::Busy),
            plakous::sessions::AgentSessionsErrorKind::StorageUnavailable => {
                Some(Self::Unavailable)
            }
            _ => None,
        }
    }

    /// The refusal class `plak.log` and the paused-wake notice name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
        }
    }
}

/// A failed read whose transient store refusal survives the string context
/// the projectors add on their way up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReadFailure {
    message: String,
    refusal: Option<StoreRefusal>,
}

impl ReadFailure {
    /// `context: error`, keeping the refusal class of `error` when it is
    /// transient.
    pub(crate) fn store(context: &str, error: &plakous::sessions::AgentSessionsError) -> Self {
        Self {
            message: format!("{context}: {error}"),
            refusal: StoreRefusal::of(error),
        }
    }
}

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for ReadFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            refusal: None,
        }
    }
}

impl From<&'static str> for ReadFailure {
    fn from(message: &'static str) -> Self {
        message.to_owned().into()
    }
}

impl From<ReadFailure> for String {
    fn from(failure: ReadFailure) -> Self {
        failure.message
    }
}

impl From<ReadFailure> for CliFailure {
    fn from(failure: ReadFailure) -> Self {
        let mut cli = Self::new(failure.message);
        cli.refusal = failure.refusal;
        cli
    }
}

impl From<crate::session_store::RecordStoreError> for CliFailure {
    fn from(error: crate::session_store::RecordStoreError) -> Self {
        let refusal = error.refusal();
        let mut cli = Self::new(String::from(error));
        cli.refusal = refusal;
        cli
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LookupFailure {
    Recovery(plakous::sessions::AgentSessionsError),
}
impl std::error::Error for CliFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.source.as_deref()? {
            LookupFailure::Recovery(error) => Some(error),
        }
    }
}
impl std::fmt::Display for CliFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl From<plakous::sessions::AgentSessionsError> for CliFailure {
    fn from(error: plakous::sessions::AgentSessionsError) -> Self {
        let mut failure = Self::new(error.to_string());
        failure.source = Some(Box::new(LookupFailure::Recovery(error)));
        failure
    }
}

/// Independent failures retained after every shutdown stage has been attempted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShutdownFailures {
    pub release: Option<CliFailure>,
    pub cleanup: Option<CliFailure>,
}

/// The tree runtime's plumbing failures are internal product failures: no
/// model asked for them and none carries a wire code.
impl From<crate::agent_tree::TreeRuntimeError> for CliFailure {
    fn from(error: crate::agent_tree::TreeRuntimeError) -> Self {
        Self::new(error.message())
    }
}

impl CliFailure {
    /// A read/preflight refusal, retaining its typed source and retry class.
    pub(crate) fn session_read(
        context: &str,
        error: plakous::sessions::AgentSessionsError,
    ) -> Self {
        let refusal = StoreRefusal::of(&error);
        let mut failure = Self::from(error).with_context(context);
        failure.refusal = refusal;
        failure
    }

    /// Internal failure with the default category.
    pub fn new(message: impl Into<String>) -> Self {
        Self::classified(CliFailureKind::Internal, message)
    }

    /// Failure with an explicit category.
    pub fn classified(kind: CliFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            shutdown: None,
            source: None,
            refusal: None,
        }
    }

    /// The transient store refusal of a failed read or refused product
    /// write, when it was one.
    #[must_use]
    pub(crate) const fn store_refusal(&self) -> Option<StoreRefusal> {
        self.refusal
    }

    /// Failure that requires explicit operator recovery (no silent bootstrap).
    pub fn recovery_required(message: impl Into<String>) -> Self {
        Self::classified(CliFailureKind::RecoveryRequired, message)
    }

    /// True when this failure is an explicit recovery-required product failure.
    #[must_use]
    pub const fn is_recovery_required(&self) -> bool {
        matches!(self.kind, CliFailureKind::RecoveryRequired)
    }

    #[must_use]
    pub fn or_kind(mut self, kind: CliFailureKind) -> Self {
        if self.kind == CliFailureKind::Internal {
            self.kind = kind;
        }
        self
    }

    pub(crate) fn with_context(mut self, context: impl std::fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }

    pub(crate) fn with_cleanup_failure(mut self, cleanup: impl std::fmt::Display) -> Self {
        self.message = format!("{}; {cleanup}", self.message);
        self
    }

    pub(crate) fn with_release_failure(mut self, release: Self) -> Self {
        self.message = format!(
            "{}; canonical session release also failed: {}",
            self.message, release.message
        );
        self.shutdown = release.shutdown.or_else(|| {
            Some(Box::new(ShutdownFailures {
                release: Some(Self::classified(release.kind, release.message)),
                cleanup: None,
            }))
        });
        self
    }

    /// Human-readable failure message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Typed shutdown failures, including canonical release and cleanup.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn shutdown_failures(&self) -> Option<&ShutdownFailures> {
        self.shutdown.as_deref()
    }

    pub(crate) fn shutdown(release: Option<Self>, cleanup: Option<Self>) -> CliResult<()> {
        let failures = ShutdownFailures { release, cleanup };
        let message = [
            ("session release failed", failures.release.as_ref()),
            ("session cleanup failed", failures.cleanup.as_ref()),
        ]
        .into_iter()
        .filter_map(|(stage, failure)| {
            failure.map(|failure| format!("{stage}: {}", failure.message()))
        })
        .collect::<Vec<_>>()
        .join("; ");
        if message.is_empty() {
            return Ok(());
        }
        let mut failure = Self::new(message);
        failure.shutdown = Some(Box::new(failures));
        Err(failure)
    }

    /// Process exit code for this failure.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        self.kind.exit_code()
    }
}

impl From<&'static str> for CliFailure {
    fn from(value: &'static str) -> Self {
        Self::new(value)
    }
}

impl From<String> for CliFailure {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Result alias for product CLI operations.
pub type CliResult<T> = Result<T, CliFailure>;

/// Refusals injected at named read sites, standing in for a store that is
/// busy, unavailable or corrupt at that moment. A process-wide queue: each
/// nextest test is its own process.
#[cfg(test)]
pub(crate) mod read_faults {
    use super::{ReadFailure, StoreRefusal};
    use std::sync::{Mutex, PoisonError};

    static FAULTS: Mutex<Vec<(&'static str, Option<StoreRefusal>)>> = Mutex::new(Vec::new());

    /// Refuses the next `times` reads at the site whose context is `site`;
    /// `None` is a permanent (integrity) refusal.
    pub(crate) fn inject(site: &'static str, refusal: Option<StoreRefusal>, times: usize) {
        let mut faults = FAULTS.lock().unwrap_or_else(PoisonError::into_inner);
        faults.extend(std::iter::repeat_n((site, refusal), times));
    }

    pub(crate) fn take(site: &str) -> Option<ReadFailure> {
        let mut faults = FAULTS.lock().unwrap_or_else(PoisonError::into_inner);
        let index = faults.iter().position(|(queued, _)| *queued == site)?;
        let (_, refusal) = faults.remove(index);
        let label = match refusal {
            Some(StoreRefusal::Busy) => "storage busy",
            Some(StoreRefusal::Unavailable) => "storage unavailable",
            None => "integrity",
        };
        Some(ReadFailure {
            message: format!("{site}: semantic session store refused the workflow: {label}"),
            refusal,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_keeps_release_and_cleanup_failures_typed() {
        let failure = CliFailure::shutdown(
            Some(CliFailure::classified(CliFailureKind::Agent, "canonical")),
            Some(CliFailure::new("cleanup")),
        )
        .unwrap_err();
        let parts = failure.shutdown_failures().unwrap();
        assert_eq!(parts.release.as_ref().unwrap().kind, CliFailureKind::Agent);
        assert_eq!(parts.cleanup.as_ref().unwrap().message(), "cleanup");
        assert!(failure.message().contains("cleanup") && failure.message().contains("canonical"));
    }

    #[test]
    fn context_preserves_the_typed_session_error() {
        let source = plakous::sessions::SessionOperationId::new("").unwrap_err();
        let failure = CliFailure::from(source.clone()).with_context("context meter after recovery");
        let retained = std::error::Error::source(&failure)
            .unwrap()
            .downcast_ref::<plakous::sessions::AgentSessionsError>()
            .unwrap();
        assert_eq!(retained, &source);
        assert!(
            failure
                .message()
                .starts_with("context meter after recovery:")
        );
    }

    #[test]
    fn recovery_required_is_distinct_from_agent_kind() {
        let failure = CliFailure::recovery_required("orchestration snapshot digest mismatch");
        assert_eq!(failure.kind, CliFailureKind::RecoveryRequired);
        assert_eq!(failure.exit_code(), 5);
        assert!(failure.is_recovery_required());
        assert!(
            !CliFailure::classified(CliFailureKind::Agent, "agent run failed")
                .is_recovery_required()
        );
        assert!(!CliFailure::new("internal").is_recovery_required());
    }
}
