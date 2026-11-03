use super::{AgentSessionsErrorKind, SessionId, SessionLocator, SessionStoreBound};
use plakous_agent::internal::facade_support::{
    SessionStoreOrigin, agent_store_origin, session_store_origin,
};
use plakous_agent::internal::{
    SessionStoreCapacity, SessionStoreError, StoreAdmission, StoreCapacityBound,
};
use plakous_agent::{AgentError, AgentErrorKind, BindingChange};
use plakous_model::StableRequestPrefixField;
use std::fmt;

/// Which share of a retention bound a refused write was held to.
///
/// `Ordinary` writes begin new work and stop first; `Settlement` writes record
/// work already running and stop inside the reserve; `Recovery` may fill the
/// bound.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStoreAdmission {
    /// New work: a root turn, a prompt, a prepare or first dispatch fence, a
    /// session creation or fork.
    Ordinary,
    /// The outcome or terminal result of work already running.
    Settlement,
    /// Explicit or open-time recovery.
    Recovery,
}

impl fmt::Display for SessionStoreAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ordinary => "ordinary",
            Self::Settlement => "settlement",
            Self::Recovery => "recovery",
        })
    }
}

/// One write refused because an aggregate retention bound has no room left
/// for its admission class. Deleting sessions frees retained state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStorePressure {
    bound: SessionStoreBound,
    used: u64,
    threshold: u64,
    limit: u64,
    admission: SessionStoreAdmission,
}

impl SessionStorePressure {
    fn from_capacity(capacity: SessionStoreCapacity) -> Option<Self> {
        let bound = match capacity.bound {
            StoreCapacityBound::Checkpoints => SessionStoreBound::Checkpoints,
            StoreCapacityBound::Objects => SessionStoreBound::Objects,
            StoreCapacityBound::ObjectBytes => SessionStoreBound::ObjectBytes,
            _ => return None,
        };
        let admission = match capacity.admission? {
            StoreAdmission::Ordinary => SessionStoreAdmission::Ordinary,
            StoreAdmission::Settlement => SessionStoreAdmission::Settlement,
            StoreAdmission::Recovery => SessionStoreAdmission::Recovery,
        };
        Some(Self {
            bound,
            used: capacity.current,
            threshold: capacity.max,
            limit: capacity.limit,
            admission,
        })
    }

    /// The storage-pressure refusal an agent failure carries, if any: a turn
    /// that failed because new work was refused, or one whose settlement was
    /// refused and that therefore ended `recovery_required`.
    #[must_use]
    pub fn from_agent_error(error: &AgentError) -> Option<Self> {
        match agent_store_origin(error)? {
            SessionStoreOrigin::Capacity(capacity) => Self::from_capacity(capacity),
            _ => None,
        }
    }

    /// Returns the exhausted bound.
    #[must_use]
    pub const fn bound(&self) -> SessionStoreBound {
        self.bound
    }

    /// Returns the retained amount the refused write would have reached.
    #[must_use]
    pub const fn used(&self) -> u64 {
        self.used
    }

    /// Returns where writes of this class stop.
    #[must_use]
    pub const fn threshold(&self) -> u64 {
        self.threshold
    }

    /// Returns the configured bound.
    #[must_use]
    pub const fn limit(&self) -> u64 {
        self.limit
    }

    /// Returns the class the refused write was held to.
    #[must_use]
    pub const fn admission(&self) -> SessionStoreAdmission {
        self.admission
    }
}

impl fmt::Display for SessionStorePressure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}/{} ({} writes stop at {})",
            self.bound, self.used, self.limit, self.admission, self.threshold
        )
    }
}

/// Redaction-safe high-level workflow failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSessionsError {
    kind: AgentSessionsErrorKind,
    message: String,
    current: Option<SessionLocator>,
    released: Option<Box<SessionLocator>>,
    session: Option<SessionId>,
    pressure: Option<Box<SessionStorePressure>>,
    binding_change: Option<Box<BindingChange>>,
}

impl AgentSessionsError {
    pub(super) fn new(kind: AgentSessionsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            current: None,
            released: None,
            session: None,
            pressure: None,
            binding_change: None,
        }
    }

    /// Names the session an incompatible-build refusal is about.
    pub(super) fn for_session(mut self, session: SessionId) -> Self {
        if self.kind == AgentSessionsErrorKind::IncompatibleBuild {
            self.message = self
                .message
                .replacen("this session", &format!("session {session}"), 1);
            self.session = Some(session);
        }
        self
    }

    pub(super) fn stale(current: SessionLocator) -> Self {
        Self {
            kind: AgentSessionsErrorKind::StaleLocator,
            message: "session locator is not the current exact branch head".to_owned(),
            current: Some(current),
            released: None,
            session: None,
            pressure: None,
            binding_change: None,
        }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> AgentSessionsErrorKind {
        self.kind
    }

    /// Returns the current exact head for stale-locator failures.
    #[must_use]
    pub const fn current_locator(&self) -> Option<SessionLocator> {
        self.current
    }

    /// The exact released head when cleanup failed after relinquishing ownership.
    #[must_use]
    pub fn released_locator(&self) -> Option<SessionLocator> {
        self.released.as_deref().copied()
    }

    pub(super) fn with_released_locator(mut self, locator: SessionLocator) -> Self {
        self.released = Some(Box::new(locator));
        self
    }

    /// Returns the session an incompatible-build refusal names, when known.
    #[must_use]
    pub const fn incompatible_session(&self) -> Option<SessionId> {
        self.session
    }

    /// Names the frozen binding input this build reconstructs differently,
    /// when an exact open or its verification refused for that reason.
    #[must_use]
    pub fn binding_change(&self) -> Option<&BindingChange> {
        self.binding_change.as_deref()
    }

    /// Returns the aggregate bound and class of a
    /// [`AgentSessionsErrorKind::StoragePressure`] refusal, when the refused
    /// write was held to an admission class.
    #[must_use]
    pub fn storage_pressure(&self) -> Option<SessionStorePressure> {
        self.pressure.as_deref().copied()
    }
}

impl fmt::Display for AgentSessionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for AgentSessionsError {}

pub(super) fn runtime_error(error: &AgentError) -> AgentSessionsError {
    let mut mapped = runtime_error_kind(error);
    mapped.binding_change = error.binding_change().cloned().map(Box::new);
    mapped
}

fn runtime_error_kind(error: &AgentError) -> AgentSessionsError {
    if let Some(origin) = agent_store_origin(error) {
        return store_origin_error(origin);
    }
    let kind = match error.kind() {
        AgentErrorKind::InvalidInput | AgentErrorKind::Model => {
            AgentSessionsErrorKind::Revalidation
        }
        AgentErrorKind::Session => AgentSessionsErrorKind::Integrity,
        AgentErrorKind::IncompatibleSession => {
            // Only bounded categories cross this boundary, never arbitrary
            // executor text or private checkpoint contents.
            let reason = match error.message() {
                "builder harness plan disagrees with durable semantics"
                | "builder harness plan disagrees with durable reconstruction" => {
                    "harness plan".to_owned()
                }
                "builder presentation disagrees with durable tool semantics" => {
                    "tool presentation".to_owned()
                }
                "durable checkpoint recovery scope is not recoverable by this build" => {
                    "recovery scope".to_owned()
                }
                message
                    if message.starts_with(
                        "runtime checkpoint recovery scope or revalidation failed:",
                    ) =>
                {
                    "runtime revalidation".to_owned()
                }
                message => stable_prefix_reason(message)
                    .unwrap_or_else(|| "runtime reconstruction".to_owned()),
            };
            let mut error = incompatible();
            error.message = format!("{} (reconstruction: {reason})", error.message);
            return error;
        }
        AgentErrorKind::OutcomeUnknown => AgentSessionsErrorKind::OutcomeUnknown,
        AgentErrorKind::Identity => AgentSessionsErrorKind::InvalidInput,
        AgentErrorKind::Backpressure | AgentErrorKind::Shutdown => {
            AgentSessionsErrorKind::StorageUnavailable
        }
        _ => AgentSessionsErrorKind::Internal,
    };
    AgentSessionsError::new(kind, error.message())
}

pub(super) fn store_error(error: SessionStoreError) -> AgentSessionsError {
    store_origin_error(session_store_origin(error))
}

/// A refusal of the SQLite store at `path`, naming the path and the remedy when
/// the cause is a permanent property of the path rather than a transient one.
///
/// The store refuses a symbolic link in any component and a database that is
/// not a private regular file, but reports both as unavailable because its
/// errors carry no paths. Only that refusal is examined, and only for causes the
/// store refuses deterministically, so a busy store still reads as busy.
#[cfg(feature = "sessions-sqlite")]
pub(super) fn store_path_error(
    path: &std::path::Path,
    error: SessionStoreError,
) -> AgentSessionsError {
    if error == SessionStoreError::BackendUnavailable
        && let Some(cause) = unsafe_store_path(path)
    {
        return AgentSessionsError::new(AgentSessionsErrorKind::UnsafeStorePath, cause);
    }
    store_error(error)
}

#[cfg(feature = "sessions-sqlite")]
fn unsafe_store_path(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt as _;

    let shown = path.display();
    let ancestors = path.ancestors().collect::<Vec<_>>();
    for ancestor in ancestors.into_iter().rev() {
        let Ok(metadata) = ancestor.symlink_metadata() else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return Some(format!(
                "session store {shown} is refused: {} is a symbolic link; \
                 use a path with no symbolic link in it (for example its resolved path)",
                ancestor.display()
            ));
        }
    }
    let metadata = path.symlink_metadata().ok()?;
    if !metadata.file_type().is_file() {
        return Some(format!(
            "session store {shown} is refused: it is not a regular file; move it aside"
        ));
    }
    (metadata.permissions().mode() & 0o077 != 0).then(|| {
        format!(
            "session store {shown} is refused: other users can access it; \
             make it private to its owner (chmod 600)"
        )
    })
}

pub(super) fn store_origin_error(origin: SessionStoreOrigin) -> AgentSessionsError {
    // The origin is content-free and is the only way an operator can tell a
    // busy/locked store apart from a corrupt or foreign one at the CLI.
    let (kind, origin_label) = match origin {
        SessionStoreOrigin::Capacity(capacity) => {
            let mut error = AgentSessionsError::new(
                AgentSessionsErrorKind::StoragePressure,
                SessionStoreError::Capacity(capacity).to_string(),
            );
            error.pressure = SessionStorePressure::from_capacity(capacity).map(Box::new);
            return error;
        }
        SessionStoreOrigin::StorageFull => {
            return AgentSessionsError::new(
                AgentSessionsErrorKind::StorageFull,
                SessionStoreError::StorageFull.to_string(),
            );
        }
        SessionStoreOrigin::NotFound => (AgentSessionsErrorKind::NotFound, "not found"),
        SessionStoreOrigin::Pruned => (
            AgentSessionsErrorKind::Pruned,
            "the checkpoint is superseded history that retention reclaimed",
        ),
        SessionStoreOrigin::IdentityConflict => (
            AgentSessionsErrorKind::IdentityConflict,
            "identity conflict",
        ),
        SessionStoreOrigin::Ownership => (
            AgentSessionsErrorKind::Ownership,
            "ownership: the session is busy; its lease is held by another owner",
        ),
        SessionStoreOrigin::NotLineHead => (
            AgentSessionsErrorKind::NotLineHead,
            "the branch is not its line's head and is read-only; continue on the head or fork it",
        ),
        SessionStoreOrigin::Busy => (AgentSessionsErrorKind::StorageBusy, "storage busy"),
        SessionStoreOrigin::Unavailable => (
            AgentSessionsErrorKind::StorageUnavailable,
            "storage unavailable",
        ),
        SessionStoreOrigin::InvalidInput => (AgentSessionsErrorKind::InvalidInput, "invalid input"),
        SessionStoreOrigin::UnsupportedPersistence => return incompatible(),
        SessionStoreOrigin::Integrity => (AgentSessionsErrorKind::Integrity, "integrity"),
    };
    AgentSessionsError::new(
        kind,
        format!("semantic session store refused the workflow: {origin_label}"),
    )
}

/// The reopen's stable-prefix mismatch, naming its differing fields only when
/// every name is a closed [`StableRequestPrefixField`] spelling.
fn stable_prefix_reason(message: &str) -> Option<String> {
    let rest = message.strip_prefix("builder semantics disagree with the durable checkpoint")?;
    if rest.is_empty() {
        return Some("stable request prefix".to_owned());
    }
    let fields = rest
        .strip_prefix(" (stable-prefix fields: ")?
        .strip_suffix(')')?;
    fields
        .split(',')
        .all(|name| {
            StableRequestPrefixField::ALL
                .iter()
                .any(|field| field.as_str() == name)
        })
        .then(|| format!("stable request prefix: {fields}"))
}

fn incompatible() -> AgentSessionsError {
    AgentSessionsError::new(
        AgentSessionsErrorKind::IncompatibleBuild,
        "this session was created by an incompatible build; start a new session",
    )
}

#[cfg(test)]
mod tests;
