use super::{AgentSessionsErrorKind, SessionId, SessionLocator, SessionStoreBound};
use plakous_agent::internal::facade_support::{
    SessionStoreOrigin, agent_store_origin, session_store_origin,
};
use plakous_agent::internal::{
    SessionStoreCapacity, SessionStoreError, StoreAdmission, StoreCapacityBound,
};
use plakous_agent::{AgentError, AgentErrorKind};
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
        SessionStoreOrigin::Unavailable => (
            AgentSessionsErrorKind::StorageUnavailable,
            "storage unavailable or busy",
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
mod tests {
    use super::*;

    #[test]
    fn workflow_unknown_is_not_an_ownership_refusal() {
        let error = runtime_error(&AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "outcome unknown",
        ));
        assert_eq!(error.kind(), AgentSessionsErrorKind::OutcomeUnknown);
    }

    #[test]
    fn ownership_refusal_names_the_busy_session_and_lease() {
        let error = store_origin_error(SessionStoreOrigin::Ownership);
        assert_eq!(error.kind(), AgentSessionsErrorKind::Ownership);
        assert!(error.to_string().contains("session is busy"));
        assert!(error.to_string().contains("lease is held by another owner"));
        assert!(!error.to_string().contains("RecoveryRequired"));
    }

    #[test]
    fn reconstruction_diagnostics_are_specific_but_never_echo_unknown_text() {
        let known = runtime_error(&AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "builder semantics disagree with the durable checkpoint",
        ));
        assert!(
            known
                .to_string()
                .ends_with("(reconstruction: stable request prefix)")
        );
        let named = runtime_error(&AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "builder semantics disagree with the durable checkpoint \
             (stable-prefix fields: instructions,tool-choice)",
        ));
        assert!(
            named
                .to_string()
                .ends_with("(reconstruction: stable request prefix: instructions,tool-choice)")
        );
        let forged = runtime_error(&AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "builder semantics disagree with the durable checkpoint \
             (stable-prefix fields: secret text)",
        ));
        assert!(
            forged
                .to_string()
                .ends_with("(reconstruction: runtime reconstruction)")
        );
        let unknown = runtime_error(&AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "private executor details must not escape",
        ));
        assert!(
            unknown
                .to_string()
                .ends_with("(reconstruction: runtime reconstruction)")
        );
        assert!(!unknown.to_string().contains("private executor"));
    }
}
