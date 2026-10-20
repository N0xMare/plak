//! Named state: the only memory that outlives a cell (spec 01 §9.8.4).
//!
//! Named state is host-owned memory of one agent incarnation, so the port
//! lives beside the local-composition seam rather than in an engine. The port
//! is the only access path. The in-memory implementation enforces the name
//! rule and the size limits before a write; a store-backed implementation adds
//! durable write-through behind the same trait.

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Mutex};

use plakous_tools::PythonHarnessSdkLimitsV1;
use serde_json::Value;

/// Boxed future of one asynchronous named-state write.
pub type NamedStateFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, NamedStateError>> + Send + 'a>>;

/// Size limits checked before a write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NamedStateLimits {
    /// Most names held at once.
    pub names: usize,
    /// Most canonical JSON bytes of one value.
    pub value_bytes: usize,
    /// Most canonical JSON bytes of all values together.
    pub total_bytes: usize,
}

impl NamedStateLimits {
    /// The named-state rows of the composition limits.
    #[must_use]
    pub fn from_limits(limits: &PythonHarnessSdkLimitsV1) -> Self {
        let size = |value: u64| usize::try_from(value).unwrap_or(usize::MAX);
        Self {
            names: size(u64::from(limits.state_names)),
            value_bytes: size(limits.state_value_bytes),
            total_bytes: size(limits.state_total_bytes),
        }
    }
}

/// One stored value.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedStateEntry {
    /// The JSON value.
    pub value: Value,
    /// Whether a cell that called an untrusted source wrote it.
    pub tainted: bool,
}

/// One listed name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedStateSlot {
    /// The name.
    pub name: String,
    /// Canonical JSON bytes of its value.
    pub bytes: u64,
    /// Its taint bit.
    pub tainted: bool,
}

/// Why a named-state operation changed nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NamedStateError {
    /// The name does not match `[A-Za-z0-9_.-]{1,64}`.
    InvalidName(String),
    /// The value is not encodable as canonical JSON.
    InvalidValue,
    /// The encoded value is over the per-value limit.
    ValueTooLarge {
        /// Encoded bytes.
        bytes: usize,
        /// The limit.
        limit: usize,
    },
    /// A new name would exceed the name limit.
    TooManyNames {
        /// The limit.
        limit: usize,
    },
    /// The write would exceed the total limit.
    TotalTooLarge {
        /// The limit.
        limit: usize,
    },
    /// The backing store failed; the outcome of the write is its own report.
    Store(String),
}

impl std::fmt::Display for NamedStateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                formatter,
                "state name {name:?} must match [A-Za-z0-9_.-]{{1,64}}"
            ),
            Self::InvalidValue => formatter.write_str("state value is not canonical JSON"),
            Self::ValueTooLarge { bytes, limit } => write!(
                formatter,
                "state value is {bytes} bytes; the limit is {limit} bytes"
            ),
            Self::TooManyNames { limit } => {
                write!(formatter, "state already holds {limit} names")
            }
            Self::TotalTooLarge { limit } => {
                write!(formatter, "state would exceed {limit} bytes in total")
            }
            Self::Store(detail) => write!(formatter, "state store failed: {detail}"),
        }
    }
}

impl std::error::Error for NamedStateError {}

/// The named-state map of one agent incarnation.
pub trait NamedStatePort: Send + Sync {
    /// The value of `name`, if any.
    ///
    /// # Errors
    ///
    /// Refuses an invalid name.
    fn get(&self, name: &str) -> Result<Option<NamedStateEntry>, NamedStateError>;
    /// Every name with its size and taint, in name order.
    fn list(&self) -> Vec<NamedStateSlot>;
    /// Stores `value` under `name`; durable before the future resolves.
    fn put(&self, name: &str, value: Value, tainted: bool) -> NamedStateFuture<'_, ()>;
    /// Removes `name`, reporting whether it existed.
    fn drop_name(&self, name: &str) -> NamedStateFuture<'_, bool>;
}

/// Checks the name rule.
///
/// # Errors
///
/// Refuses a name outside `[A-Za-z0-9_.-]{1,64}`.
pub fn validate_state_name(name: &str) -> Result<(), NamedStateError> {
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(NamedStateError::InvalidName(name.to_owned()))
    }
}

struct Stored {
    value: Value,
    bytes: usize,
    tainted: bool,
}

/// Process-memory named state with the composition's limits.
pub struct InMemoryNamedState {
    limits: NamedStateLimits,
    values: Mutex<BTreeMap<String, Stored>>,
}

impl InMemoryNamedState {
    /// An empty map under `limits`.
    #[must_use]
    pub fn new(limits: NamedStateLimits) -> Self {
        Self {
            limits,
            values: Mutex::new(BTreeMap::new()),
        }
    }

    fn values(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Stored>> {
        self.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn put_now(&self, name: &str, value: Value, tainted: bool) -> Result<(), NamedStateError> {
        validate_state_name(name)?;
        let bytes = serde_json_canonicalizer::to_vec(&value)
            .map_err(|_| NamedStateError::InvalidValue)?
            .len();
        if bytes > self.limits.value_bytes {
            return Err(NamedStateError::ValueTooLarge {
                bytes,
                limit: self.limits.value_bytes,
            });
        }
        let mut values = self.values();
        let replaced = values.get(name).map_or(0, |stored| stored.bytes);
        if !values.contains_key(name) && values.len() >= self.limits.names {
            return Err(NamedStateError::TooManyNames {
                limit: self.limits.names,
            });
        }
        let total = values.values().map(|stored| stored.bytes).sum::<usize>() - replaced + bytes;
        if total > self.limits.total_bytes {
            return Err(NamedStateError::TotalTooLarge {
                limit: self.limits.total_bytes,
            });
        }
        values.insert(
            name.to_owned(),
            Stored {
                value,
                bytes,
                tainted,
            },
        );
        Ok(())
    }
}

impl NamedStatePort for InMemoryNamedState {
    fn get(&self, name: &str) -> Result<Option<NamedStateEntry>, NamedStateError> {
        validate_state_name(name)?;
        Ok(self.values().get(name).map(|stored| NamedStateEntry {
            value: stored.value.clone(),
            tainted: stored.tainted,
        }))
    }

    fn list(&self) -> Vec<NamedStateSlot> {
        self.values()
            .iter()
            .map(|(name, stored)| NamedStateSlot {
                name: name.clone(),
                bytes: u64::try_from(stored.bytes).unwrap_or(u64::MAX),
                tainted: stored.tainted,
            })
            .collect()
    }

    fn put(&self, name: &str, value: Value, tainted: bool) -> NamedStateFuture<'_, ()> {
        let result = self.put_now(name, value, tainted);
        Box::pin(async move { result })
    }

    fn drop_name(&self, name: &str) -> NamedStateFuture<'_, bool> {
        let result = validate_state_name(name).map(|()| self.values().remove(name).is_some());
        Box::pin(async move { result })
    }
}
