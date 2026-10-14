//! Opaque identity and injectable time primitives.

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// An error raised while constructing an identity or timestamp.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum IdentityError {
    /// An identifier was empty.
    #[error("identifier cannot be empty")]
    Empty,
    /// An identifier exceeded the portable size limit.
    #[error("identifier exceeds {maximum} bytes")]
    TooLong {
        /// The maximum accepted byte length.
        maximum: usize,
    },
    /// An identifier contained a non-portable character.
    #[error("identifier contains a non-portable character at byte {index}")]
    InvalidCharacter {
        /// The byte index of the invalid character.
        index: usize,
    },
    /// A UUID was nil.
    #[error("nil UUIDs are not valid Plakous identities")]
    NilUuid,
    /// UUID text was malformed.
    #[error("invalid UUID: {0}")]
    InvalidUuid(String),
    /// A deterministic UUID source was exhausted.
    #[error("UUID source is exhausted")]
    Exhausted,
    /// Nanoseconds were outside their valid range.
    #[error("nanoseconds must be less than 1,000,000,000")]
    InvalidNanoseconds,
}

/// A serializable UTC timestamp independent of the local timezone.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Timestamp {
    seconds: u64,
    nanoseconds: u32,
}

#[derive(Deserialize)]
struct TimestampWire {
    seconds: u64,
    nanoseconds: u32,
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TimestampWire::deserialize(deserializer)?;
        Self::new(wire.seconds, wire.nanoseconds).map_err(serde::de::Error::custom)
    }
}

impl Timestamp {
    /// Constructs a timestamp after the Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::InvalidNanoseconds`] when `nanoseconds` is not
    /// a valid subsecond component.
    pub fn new(seconds: u64, nanoseconds: u32) -> Result<Self, IdentityError> {
        if nanoseconds >= 1_000_000_000 {
            return Err(IdentityError::InvalidNanoseconds);
        }
        Ok(Self {
            seconds,
            nanoseconds,
        })
    }

    /// Returns whole seconds since the Unix epoch.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        self.seconds
    }

    /// Returns the subsecond nanosecond component.
    #[must_use]
    pub const fn nanoseconds(self) -> u32 {
        self.nanoseconds
    }
}

/// The current Unix time in whole seconds.
///
/// The one shared spelling for wall-clock stamps that cannot carry an injected
/// [`Clock`]. Returns zero for a system clock set before the epoch. Callers
/// that stamp durations rather than instants want [`now_unix_millis`]; the two
/// units never share a name.
#[must_use]
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The current Unix time in whole milliseconds.
///
/// Millisecond resolution so elapsed measurements are real rather than whole
/// seconds. Returns zero for a system clock set before the epoch and saturates
/// at [`u64::MAX`] beyond the representable range.
#[must_use]
pub fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// An injectable source of UTC time.
pub trait Clock: Send + Sync + 'static {
    /// Returns the source's current UTC timestamp.
    fn now(&self) -> Timestamp;
}

/// A clock backed by [`SystemTime`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp {
            seconds: duration.as_secs(),
            nanoseconds: duration.subsec_nanos(),
        }
    }
}

/// A deterministic clock useful in tests and replay.
#[derive(Clone, Copy, Debug)]
pub struct FixedClock {
    timestamp: Timestamp,
}

impl FixedClock {
    /// Creates a clock that always returns `timestamp`.
    #[must_use]
    pub const fn new(timestamp: Timestamp) -> Self {
        Self { timestamp }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.timestamp
    }
}

/// An injectable source of UUID values.
pub trait IdGenerator: Send + Sync + 'static {
    /// Generates a UUID using the supplied time as its temporal input.
    ///
    /// # Errors
    ///
    /// Returns an identity-source error when generation cannot continue.
    fn next_uuid(&self, now: Timestamp) -> Result<Uuid, IdentityError>;
}

/// A `UUIDv7` generator suitable for production identities.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemIdGenerator;

impl IdGenerator for SystemIdGenerator {
    fn next_uuid(&self, now: Timestamp) -> Result<Uuid, IdentityError> {
        let timestamp =
            uuid::Timestamp::from_unix(uuid::NoContext, now.seconds(), now.nanoseconds());
        Ok(Uuid::new_v7(timestamp))
    }
}

/// A deterministic, finite UUID source.
#[derive(Debug)]
pub struct SequenceIdGenerator {
    values: Mutex<VecDeque<Uuid>>,
}

impl SequenceIdGenerator {
    /// Creates a source that yields UUIDs in iteration order.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::NilUuid`] if any supplied value is nil.
    pub fn new(values: impl IntoIterator<Item = Uuid>) -> Result<Self, IdentityError> {
        let values = values.into_iter().collect::<VecDeque<_>>();
        if values.iter().any(Uuid::is_nil) {
            return Err(IdentityError::NilUuid);
        }
        Ok(Self {
            values: Mutex::new(values),
        })
    }
}

impl IdGenerator for SequenceIdGenerator {
    fn next_uuid(&self, _now: Timestamp) -> Result<Uuid, IdentityError> {
        let mut values = match self.values.lock() {
            Ok(values) => values,
            Err(poisoned) => poisoned.into_inner(),
        };
        values.pop_front().ok_or(IdentityError::Exhausted)
    }
}

macro_rules! opaque_uuid {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(try_from = "Uuid")]
        pub struct $name(Uuid);

        impl $name {
            /// Generates an identity with injected time and UUID sources.
            ///
            /// # Errors
            ///
            /// Returns an error from the injected source or when it emits a nil UUID.
            pub fn generate(
                clock: &dyn Clock,
                generator: &dyn IdGenerator,
            ) -> Result<Self, IdentityError> {
                Self::try_from(generator.next_uuid(clock.now())?)
            }

            /// Returns the opaque UUID value.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = IdentityError;

            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                if value.is_nil() {
                    Err(IdentityError::NilUuid)
                } else {
                    Ok(Self(value))
                }
            }
        }

        impl FromStr for $name {
            type Err = IdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let uuid = Uuid::parse_str(value)
                    .map_err(|error| IdentityError::InvalidUuid(error.to_string()))?;
                Self::try_from(uuid)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

opaque_uuid!(AgentId, "An opaque agent identity.");
opaque_uuid!(SessionId, "An opaque model-session identity.");
opaque_uuid!(BranchId, "An opaque conversation-branch identity.");
opaque_uuid!(RunId, "An opaque run identity.");
opaque_uuid!(TurnId, "An opaque turn identity.");
opaque_uuid!(CallId, "An opaque canonical tool-call identity.");
opaque_uuid!(AttemptId, "An opaque model-attempt identity.");
opaque_uuid!(EventId, "An opaque event identity.");
opaque_uuid!(EntryId, "An opaque conversation-entry identity.");
opaque_uuid!(CheckpointId, "An opaque committed-checkpoint identity.");

/// Fixed byte width of one session-workflow model-operation identity.
pub const SESSION_WORKFLOW_ROOT_ID_BYTES: usize = 32;

/// A nonzero, exact, opaque root for one high-level session workflow.
///
/// Unlike a turn identity this value is not a UUID and carries no timestamp or
/// provider meaning. Its fixed-width bytes are caller-stable across restart.
#[derive(Clone, Copy, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(
    try_from = "[u8; SESSION_WORKFLOW_ROOT_ID_BYTES]",
    into = "[u8; SESSION_WORKFLOW_ROOT_ID_BYTES]"
)]
pub struct SessionWorkflowRootId([u8; SESSION_WORKFLOW_ROOT_ID_BYTES]);

impl SessionWorkflowRootId {
    /// Creates a nonzero exact workflow root.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::Empty`] for the reserved all-zero value.
    pub fn new(bytes: [u8; SESSION_WORKFLOW_ROOT_ID_BYTES]) -> Result<Self, IdentityError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(IdentityError::Empty);
        }
        Ok(Self(bytes))
    }

    /// Returns the exact opaque bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; SESSION_WORKFLOW_ROOT_ID_BYTES] {
        &self.0
    }
}

impl TryFrom<[u8; SESSION_WORKFLOW_ROOT_ID_BYTES]> for SessionWorkflowRootId {
    type Error = IdentityError;

    fn try_from(value: [u8; SESSION_WORKFLOW_ROOT_ID_BYTES]) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SessionWorkflowRootId> for [u8; SESSION_WORKFLOW_ROOT_ID_BYTES] {
    fn from(value: SessionWorkflowRootId) -> Self {
        value.0
    }
}

impl fmt::Debug for SessionWorkflowRootId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SessionWorkflowRootId")
            .field(&"<opaque>")
            .finish()
    }
}

/// Closed logical root of one exact model operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ModelOperationRoot {
    /// An ordinary admitted root turn.
    Turn(TurnId),
    /// A high-level session workflow which must not fabricate a turn.
    SessionWorkflow(SessionWorkflowRootId),
}

impl ModelOperationRoot {
    /// Returns the turn only when this is a turn-rooted operation.
    #[must_use]
    pub const fn turn_id(self) -> Option<TurnId> {
        match self {
            Self::Turn(turn_id) => Some(turn_id),
            Self::SessionWorkflow(_) => None,
        }
    }

    /// Returns the workflow only when this is a session-workflow operation.
    #[must_use]
    pub const fn session_workflow_id(self) -> Option<SessionWorkflowRootId> {
        match self {
            Self::Turn(_) => None,
            Self::SessionWorkflow(workflow_id) => Some(workflow_id),
        }
    }
}

fn validate_identifier(value: &str) -> Result<(), IdentityError> {
    if value.is_empty() {
        return Err(IdentityError::Empty);
    }
    if value.len() > 128 {
        return Err(IdentityError::TooLong { maximum: 128 });
    }
    if let Some((index, _)) = value.bytes().enumerate().find(|(_, byte)| {
        !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err(IdentityError::InvalidCharacter { index });
    }
    Ok(())
}

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Creates a validated identifier.
            ///
            /// # Errors
            ///
            /// Returns an error when the value is empty, oversized, or contains a
            /// character outside the portable identifier alphabet.
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                validate_identifier(&value)?;
                Ok(Self(value))
            }

            /// Returns the identifier text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentityError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

string_id!(ProtocolId, "A stable protocol-dialect identifier.");
string_id!(RouteId, "A stable configured route identifier.");
string_id!(ModelId, "A stable logical or remote model identifier.");
string_id!(ModelFamilyId, "A stable model-family identifier.");
string_id!(
    ModelAdapterProfileId,
    "A stable model-adapter/admission profile identifier."
);
string_id!(McpServerId, "A stable remote tool-server identifier.");
string_id!(ToolName, "A stable model-facing tool name.");
string_id!(
    ToolResourceKey,
    "A stable scheduler resource identity shared by mutually exclusive tools."
);
string_id!(NamespaceId, "A stable native-option namespace identifier.");
string_id!(ReplayKindId, "A stable replay-payload kind identifier.");
string_id!(
    ProviderProfileRevisionId,
    "A stable immutable provider-profile revision identifier."
);
string_id!(
    CredentialSelectionId,
    "A stable non-secret credential-selection reference."
);
string_id!(
    ProviderAuthorityClassId,
    "A stable provider authority and billing class identifier."
);
string_id!(
    ProviderServiceTierId,
    "A stable exact provider service-tier identifier."
);

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_use_injected_sources() -> Result<(), IdentityError> {
        let timestamp = Timestamp::new(42, 7)?;
        let uuid = Uuid::from_u128(1);
        let generator = SequenceIdGenerator::new([uuid])?;
        let id = RunId::generate(&FixedClock::new(timestamp), &generator)?;
        assert_eq!(id.as_uuid(), uuid);
        assert_eq!(
            generator.next_uuid(timestamp),
            Err(IdentityError::Exhausted)
        );
        Ok(())
    }

    #[test]
    fn identifiers_are_validated_during_deserialization() {
        let result = serde_json::from_str::<ProtocolId>("\"contains a space\"");
        assert!(result.is_err());
    }

    #[test]
    fn timestamp_deserialization_validates_nanoseconds() {
        let result = serde_json::from_str::<Timestamp>(r#"{"seconds":1,"nanoseconds":1000000000}"#);
        assert!(result.is_err());
    }
}
