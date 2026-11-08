//! Product CLI failure taxonomy shared between the host runtime and Plak application.

/// Terminal outcome for CLI workflows that may be interrupted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOutcome {
    /// Workflow completed successfully.
    Success,
    /// Workflow was interrupted before completion.
    Interrupted,
    /// Quit exhausted its terminal join and selected lease-only abandonment.
    EscalatedQuit,
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
    /// The agent tree is in another live host process's custody
    /// (`tree_hosted_elsewhere`).
    HostedElsewhere,
    /// An embedded attachment cannot take a daemon-held process line.
    LocalRunnerProcesses,
    /// An embedded attachment cannot resolve a recorded root or child process line.
    LocalRunnerInspection,
    /// A standalone import cannot bind runners while a host owns the home.
    ImportThroughRunningHost,
}

impl CliFailureKind {
    /// Exit code for this failure category.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Internal => 1,
            Self::Usage | Self::ImportThroughRunningHost => 2,
            Self::Auth => 3,
            Self::Agent | Self::RecoveryRequired => 5,
            Self::HostedElsewhere | Self::LocalRunnerProcesses | Self::LocalRunnerInspection => 7,
        }
    }
}

/// Structured CLI failure with an exit-code category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliFailure {
    wire_refusal: Option<Box<plakous_host_protocol::wire::RequestRefusal>>,
    kind: CliFailureKind,
    message: String,
    shutdown: Option<Box<ShutdownFailures>>,
    source: Option<Box<LookupFailure>>,
    refusal: Option<StoreRefusal>,
    remedy: Option<plakous_host_protocol::notice_remedy::NoticeRemedy>,
    attach_failure: AttachFailure,
    advice: Option<Box<plakous_host_protocol::refusal::RefusalAdvice>>,
    origin: Option<Box<plakous_host_protocol::status::HostFailureOrigin>>,
}

/// A semantic- or product-store refusal that the same call may clear when
/// retried: nothing was written and no authority was lost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreRefusal {
    /// Another connection held the store, or its request queue was full.
    Busy,
    /// The store could not be reached or was shutting down.
    Unavailable,
    /// The product volume is below its new-work admission floor.
    LowSpace,
    /// An authoritative write refused for exhausted storage, without committing.
    StorageFull,
    /// Another live host process holds the agent tree's custody
    /// (`tree_hosted_elsewhere`); nothing was written. The message is the
    /// refusal itself.
    HostedElsewhere,
}

impl StoreRefusal {
    fn of(error: &plakous::sessions::AgentSessionsError) -> Option<Self> {
        match error.kind() {
            plakous::sessions::AgentSessionsErrorKind::StorageBusy => Some(Self::Busy),
            plakous::sessions::AgentSessionsErrorKind::StorageUnavailable => {
                Some(Self::Unavailable)
            }
            plakous::sessions::AgentSessionsErrorKind::StorageFull => Some(Self::StorageFull),
            _ => None,
        }
    }

    /// The refusal class `plak.log` and the paused-wake notice name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
            Self::LowSpace => "low-space",
            Self::StorageFull => "storage-full",
            Self::HostedElsewhere => "hosted-elsewhere",
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

impl ReadFailure {
    /// A failure that keeps `refusal` as its class.
    pub(crate) const fn refused(refusal: StoreRefusal, message: String) -> Self {
        Self {
            message,
            refusal: Some(refusal),
        }
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

impl From<crate::session_store::RecordStoreError> for ReadFailure {
    fn from(error: crate::session_store::RecordStoreError) -> Self {
        Self {
            refusal: error.refusal(),
            message: error.to_string(),
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
        if failure.refusal == Some(StoreRefusal::HostedElsewhere) {
            return Self::hosted_elsewhere(failure.message);
        }
        let mut cli = Self::new(failure.message);
        cli.refusal = failure.refusal;
        if failure.refusal == Some(StoreRefusal::LowSpace) {
            cli = cli.with_advice(plakous_host_protocol::refusal::RefusalAdvice::CheckStorage);
        }
        cli
    }
}

impl From<crate::session_store::RecordStoreError> for CliFailure {
    fn from(error: crate::session_store::RecordStoreError) -> Self {
        let refusal = error.refusal();
        if refusal == Some(StoreRefusal::HostedElsewhere) {
            return Self::hosted_elsewhere(String::from(error));
        }
        let mut cli = Self::new(String::from(error));
        cli.refusal = refusal;
        if refusal == Some(StoreRefusal::LowSpace) {
            cli = cli.with_advice(plakous_host_protocol::refusal::RefusalAdvice::CheckStorage);
        }
        if refusal == Some(StoreRefusal::StorageFull) {
            cli.remedy = Some(plakous_host_protocol::notice_remedy::NoticeRemedy::FreeDiskSpace {});
        }
        cli
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LookupFailure {
    Recovery(plakous::sessions::AgentSessionsError),
    Capability(crate::capabilities::CapabilityUnavailable),
    Account(crate::capabilities::AccountAdmissionError),
}
impl std::error::Error for CliFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.source.as_deref()? {
            LookupFailure::Recovery(error) => Some(error),
            LookupFailure::Capability(error) => Some(error),
            LookupFailure::Account(error) => Some(error),
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
        let mut failure = Self::new(collapse_repeated_causes(&error.to_string()));
        failure.remedy = crate::storage_pressure::sessions_remedy(&error);
        failure.advice = incompatible_session_advice(&error).map(Box::new);
        failure.attach_failure = sessions_attach_failure(&error);
        failure.source = Some(Box::new(LookupFailure::Recovery(error)));
        failure
    }
}

/// An open refused for a reconstructed frozen binding input is a binding
/// change; a missing login or any other refusal never is.
pub(crate) fn sessions_attach_failure(
    error: &plakous::sessions::AgentSessionsError,
) -> AttachFailure {
    if error.binding_change().is_some() {
        AttachFailure::BindingChanged
    } else {
        AttachFailure::default()
    }
}

pub(crate) fn incompatible_session_advice(
    error: &plakous::sessions::AgentSessionsError,
) -> Option<plakous_host_protocol::refusal::RefusalAdvice> {
    error.incompatible_session().map(|id| {
        plakous_host_protocol::refusal::RefusalAdvice::ExportSession {
            session_id: plakous_host_protocol::notice_remedy::ProductSessionId::from_uuid(
                id.as_uuid(),
            ),
        }
    })
}

impl From<crate::capabilities::CapabilityUnavailable> for CliFailure {
    fn from(error: crate::capabilities::CapabilityUnavailable) -> Self {
        let mut failure = Self::classified(
            CliFailureKind::Usage,
            format!(
                "{error}; available: {}",
                crate::capabilities::compiled_features().join(", ")
            ),
        );
        failure.source = Some(Box::new(LookupFailure::Capability(error)));
        failure
    }
}

impl From<crate::capabilities::AccountAdmissionError> for CliFailure {
    fn from(error: crate::capabilities::AccountAdmissionError) -> Self {
        if let crate::capabilities::AccountAdmissionError::CapabilityUnavailable(error) = error {
            return Self::from(error);
        }
        let kind = if matches!(
            error,
            crate::capabilities::AccountAdmissionError::AccountUnavailable { .. }
                | crate::capabilities::AccountAdmissionError::AccountBusy { .. }
                | crate::capabilities::AccountAdmissionError::AccountSignedIn { .. }
        ) {
            CliFailureKind::Auth
        } else {
            CliFailureKind::Usage
        };
        let mut failure = Self::classified(kind, error.to_string());
        failure.source = Some(Box::new(LookupFailure::Account(error)));
        failure
    }
}

impl From<crate::runtime::TreeWriteError> for CliFailure {
    fn from(error: crate::runtime::TreeWriteError) -> Self {
        match error {
            crate::runtime::TreeWriteError::HostedElsewhere(refusal) => {
                Self::hosted_elsewhere(refusal.to_string())
            }
            crate::runtime::TreeWriteError::Refused(error) => Self::new(error.message),
        }
    }
}

/// Independent failures retained after every shutdown stage has been attempted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShutdownFailures {
    pub release: Option<CliFailure>,
    pub cleanup: Option<CliFailure>,
}

/// The tree runtime's plumbing failures are internal product failures: no
/// model asked for them and none carries a wire code. The one typed refusal
/// among them, another host process's custody of the tree, keeps its kind.
impl From<crate::agent_tree::TreeRuntimeError> for CliFailure {
    fn from(error: crate::agent_tree::TreeRuntimeError) -> Self {
        if error.hosted_elsewhere_refusal().is_some() {
            return Self::hosted_elsewhere(error.message());
        }
        Self::new(error.message())
    }
}

impl CliFailure {
    /// Whether a known capability is omitted, rather than an unknown account.
    #[must_use]
    pub fn is_capability_unavailable(&self) -> bool {
        matches!(self.source.as_deref(), Some(LookupFailure::Capability(_)))
    }

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

    /// Another live host process holds the agent tree: exit code 7, the
    /// stable `tree_hosted_elsewhere` code and its attach cause. `message` is
    /// the refusal itself.
    #[must_use]
    pub fn hosted_elsewhere(message: impl Into<String>) -> Self {
        Self::classified(CliFailureKind::HostedElsewhere, message)
            .with_attach_failure(AttachFailure::TreeHostedElsewhere)
    }

    /// Refuses an embedded attachment without opening or recovering its line.
    #[must_use]
    pub fn local_runner_processes() -> Self {
        Self::classified(
            CliFailureKind::LocalRunnerProcesses,
            "this session's processes run in the local runner; start `plak host`, or see them with `plak runner ps`",
        )
    }

    pub(crate) fn local_runner_inspection(
        agent: &str,
        locator: &str,
        cause: impl std::fmt::Display,
    ) -> Self {
        Self::classified(
            CliFailureKind::LocalRunnerInspection,
            format!(
                "cannot inspect local-runner ownership for node {agent} (locator {locator}): {cause}; open it through `plak host` (start `plak host`, then attach), or inspect daemon jobs with `plak runner ps`; only as a destructive last resort, use `plak session delete` with `--abandon-children` when needed"
            ),
        )
    }

    /// Internal failure with the default category.
    pub fn new(message: impl Into<String>) -> Self {
        Self::classified(CliFailureKind::Internal, message)
    }

    /// Failure with an explicit category.
    pub fn classified(kind: CliFailureKind, message: impl Into<String>) -> Self {
        Self {
            wire_refusal: None,
            kind,
            message: message.into(),
            shutdown: None,
            source: None,
            refusal: None,
            remedy: None,
            attach_failure: AttachFailure::default(),
            advice: None,
            origin: None,
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
        Self::classified(
            CliFailureKind::RecoveryRequired,
            collapse_repeated_causes(&message.into()),
        )
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

    /// Prefix product context without losing the typed failure or its remedy.
    #[must_use]
    pub fn with_context(mut self, context: impl std::fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }

    pub(crate) fn with_cleanup_failure(mut self, cleanup: impl std::fmt::Display) -> Self {
        self.message = collapse_repeated_causes(&format!("{}; {cleanup}", self.message));
        self
    }

    pub(crate) fn with_release_failure(mut self, release: Self) -> Self {
        self.message = collapse_repeated_causes(&format!(
            "{}; canonical session release also failed: {}",
            self.message, release.message
        ));
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

    /// Retain an authenticated host refusal across a synchronous session opener.
    /// This is process-local response metadata, never persisted or exported.
    #[must_use]
    pub fn with_wire_refusal(
        mut self,
        refusal: plakous_host_protocol::wire::RequestRefusal,
    ) -> Self {
        self.wire_refusal = Some(Box::new(refusal));
        self
    }

    /// The typed host refusal, without reconstructing it from rendered text.
    #[must_use]
    pub fn wire_refusal(&self) -> Option<&plakous_host_protocol::wire::RequestRefusal> {
        self.wire_refusal.as_deref()
    }

    /// Attach transient typed refusal facts without rendering frontend text.
    #[must_use]
    pub fn with_optional_advice(
        mut self,
        advice: Option<plakous_host_protocol::refusal::RefusalAdvice>,
    ) -> Self {
        self.advice = advice.map(Box::new);
        self
    }

    /// Preserve a semantic repair independently of diagnostic context.
    #[must_use]
    pub fn with_remedy(
        mut self,
        remedy: plakous_host_protocol::notice_remedy::NoticeRemedy,
    ) -> Self {
        self.remedy = Some(remedy);
        self
    }

    /// The semantic remedy retained at the originating failure site.
    #[must_use]
    pub fn remedy(&self) -> Option<&plakous_host_protocol::notice_remedy::NoticeRemedy> {
        self.remedy.as_ref()
    }

    /// Preserve the structured action-refusal facts the product needs to render help.
    #[must_use]
    pub fn with_advice(mut self, advice: plakous_host_protocol::refusal::RefusalAdvice) -> Self {
        self.advice = Some(Box::new(advice));
        self
    }

    /// Structured action-refusal facts, independent of diagnostic prose.
    #[must_use]
    pub fn advice(&self) -> Option<&plakous_host_protocol::refusal::RefusalAdvice> {
        self.advice.as_deref()
    }

    /// Retain an optional originating semantic remedy when composing a failure.
    #[must_use]
    pub fn with_optional_remedy(
        mut self,
        remedy: Option<&plakous_host_protocol::notice_remedy::NoticeRemedy>,
    ) -> Self {
        self.remedy = remedy.cloned();
        self
    }

    /// Bounded typed startup/status failure, without rendering frontend instructions.
    #[must_use]
    pub fn host_failure(&self) -> HostFailure {
        HostFailure {
            diagnostic: DiagnosticText::new(self.message.clone()),
            remedy: self.remedy.clone(),
            cause: self.attach_failure,
            advice: self.advice.clone(),
            origin: self.origin.clone(),
        }
    }

    /// Retain the attach condition at its typed originating source.
    #[must_use]
    pub const fn with_attach_failure(mut self, cause: AttachFailure) -> Self {
        self.attach_failure = cause;
        self
    }

    /// Preserve binding source facts for frontend status projection.
    #[must_use]
    pub fn with_origin(mut self, origin: plakous_host_protocol::status::HostFailureOrigin) -> Self {
        self.origin = Some(Box::new(origin));
        self
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
        let mut failure = Self::new(collapse_repeated_causes(&message));
        failure.shutdown = Some(Box::new(failures));
        Err(failure)
    }

    /// Process exit code for this failure.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        self.kind.exit_code()
    }

    /// The stable refusal code a machine reader keys on, for a failure that
    /// has one.
    #[must_use]
    pub const fn code(&self) -> Option<&'static str> {
        match self.kind {
            CliFailureKind::HostedElsewhere => Some("tree_hosted_elsewhere"),
            CliFailureKind::LocalRunnerProcesses => Some("local_runner_processes"),
            CliFailureKind::LocalRunnerInspection => Some("local_runner_inspection"),
            CliFailureKind::ImportThroughRunningHost => Some("import_through_running_host"),
            _ => None,
        }
    }
}

/// Keep every stage, but print an identical leaf cause only once. This is
/// presentation only: the failure kind, typed source and shutdown parts stay intact.
pub(crate) fn collapse_repeated_causes(message: &str) -> String {
    let mut leaves = Vec::new();
    let mut originals = Vec::new();
    let mut output = Vec::new();
    for clause in message.split("; ") {
        if originals.contains(&clause) {
            continue;
        }
        originals.push(clause);
        let (stage, reason) = clause
            .rsplit_once(": ")
            .map_or((None, clause), |(stage, cause)| (Some(stage), cause));
        let scalar = reason.parse::<f64>().is_ok() || matches!(reason, "true" | "false" | "null");
        let compressed = if !reason.is_empty() && !scalar && leaves.contains(&reason) {
            stage.map_or_else(
                || "[same cause]".to_owned(),
                |stage| format!("{stage}: [same cause]"),
            )
        } else {
            leaves.push(reason);
            clause.to_owned()
        };
        if output.contains(&compressed) {
            output.push(clause.to_owned());
        } else {
            output.push(compressed);
        }
    }
    output.join("; ")
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
            Some(StoreRefusal::LowSpace) => "storage low space",
            Some(StoreRefusal::StorageFull) => "storage full",
            Some(StoreRefusal::HostedElsewhere) => "hosted elsewhere",
            None => "integrity",
        };
        Some(ReadFailure {
            message: format!("{site}: semantic session store refused the workflow: {label}"),
            refusal,
        })
    }
}

#[cfg(test)]
mod tests;

use plakous_host_protocol::status::{AttachFailure, DiagnosticText, HostFailure};
