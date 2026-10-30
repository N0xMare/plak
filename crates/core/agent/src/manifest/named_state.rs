//! Bounded canonical JSON owned by a session, never by an evaluator generation.

use std::{collections::BTreeMap, sync::Arc};

use serde_json::Value;
use thiserror::Error;

use crate::checkpoint_bundle::{RuntimeObjectAddress, RuntimeObjectKind, object_address};

use super::RuntimeManifestError;

pub(crate) const MAX_STATE_KEYS: usize = 64;
pub(crate) const MAX_STATE_VALUE_BYTES: usize = 1024 * 1024;
const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

/// A refused state request; no part of the request has been published.
#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum StateError {
    /// The key does not match the closed state-key grammar.
    #[error("invalid named-state key")]
    InvalidKey,
    /// The canonical JSON value exceeds 1 MiB.
    #[error("named-state value has {bytes} bytes (maximum 1 MiB)")]
    ValueTooLarge {
        /// Canonical JSON byte size of the refused value.
        bytes: usize,
    },
    /// The map would contain more than 64 keys.
    #[error("too many named-state keys (maximum 64)")]
    TooManyKeys,
    /// The map's canonical values exceed 4 MiB in total.
    #[error("named state has {bytes} bytes (maximum 4 MiB)")]
    StateTooLarge {
        /// Total canonical JSON byte size of the refused map.
        bytes: usize,
    },
    /// An integer is outside the exactly interoperable IEEE-double range.
    #[error("named-state integers must be within -(2^53-1)..=2^53-1")]
    InexactNumber,
    /// The request or result belongs to no active cell.
    #[error("named-state request belongs to no active cell")]
    ForeignCell,
    /// JSON parsing or canonicalization refused the value.
    #[error("invalid named-state JSON")]
    Json(#[source] Arc<serde_json::Error>),
    /// Persisted JSON bytes are not their canonical encoding.
    #[error("noncanonical named-state JSON")]
    NonCanonical,
    /// The immutable value could not be content-addressed.
    #[error("invalid named-state object address")]
    Address(#[source] crate::CheckpointBundleError),
}

impl From<StateError> for RuntimeManifestError {
    fn from(source: StateError) -> Self {
        Self::InvalidNamedState(Box::new(source))
    }
}

impl From<serde_json::Error> for StateError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(Arc::new(source))
    }
}

// Manifest errors remain cloneable and comparable without flattening the
// original JSON error out of their source chain.
impl PartialEq for StateError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InvalidKey, Self::InvalidKey)
            | (Self::TooManyKeys, Self::TooManyKeys)
            | (Self::InexactNumber, Self::InexactNumber)
            | (Self::ForeignCell, Self::ForeignCell)
            | (Self::NonCanonical, Self::NonCanonical) => true,
            (Self::ValueTooLarge { bytes: left }, Self::ValueTooLarge { bytes: right })
            | (Self::StateTooLarge { bytes: left }, Self::StateTooLarge { bytes: right }) => {
                left == right
            }
            (Self::Json(left), Self::Json(right)) => {
                left.classify() == right.classify()
                    && left.line() == right.line()
                    && left.column() == right.column()
                    && left.to_string() == right.to_string()
            }
            (Self::Address(left), Self::Address(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for StateError {}

/// Immutable, validated named values in a runtime manifest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NamedState {
    values: BTreeMap<String, Arc<[u8]>>,
    addresses: BTreeMap<String, RuntimeObjectAddress>,
}

impl NamedState {
    /// Returns the canonical JSON bytes, ordered by key.
    #[must_use]
    pub const fn values(&self) -> &BTreeMap<String, Arc<[u8]>> {
        &self.values
    }

    /// Returns a value, distinguishing a missing key from JSON null.
    pub fn get(&self, key: &str) -> Result<Option<Value>, StateError> {
        validate_state_key(key)?;
        self.values
            .get(key)
            .map(|bytes| decode_state_value(bytes))
            .transpose()
    }

    /// Replaces one value without modifying this snapshot on refusal.
    pub fn put(&self, key: String, value: &Value) -> Result<Self, StateError> {
        validate_state_key(&key)?;
        validate_numbers(value)?;
        let bytes = serde_json_canonicalizer::to_vec(value).map_err(StateError::from)?;
        validate_state_value(&bytes)?;
        let mut values = self.values.clone();
        values.insert(key, bytes.into());
        Self::validated_values(values)
    }

    /// Removes a key without modifying this snapshot.
    pub fn drop_key(&self, key: &str) -> Result<Self, StateError> {
        validate_state_key(key)?;
        let mut state = self.clone();
        state.values.remove(key);
        state.addresses.remove(key);
        Ok(state)
    }

    pub(crate) fn from_values(
        values: BTreeMap<String, Arc<[u8]>>,
    ) -> Result<Self, RuntimeManifestError> {
        Self::validated_values(values).map_err(Into::into)
    }

    fn validated_values(values: BTreeMap<String, Arc<[u8]>>) -> Result<Self, StateError> {
        if values.len() > MAX_STATE_KEYS {
            return Err(StateError::TooManyKeys);
        }
        let bytes = values.values().map(|value| value.len()).sum::<usize>();
        if bytes > MAX_STATE_BYTES {
            return Err(StateError::StateTooLarge { bytes });
        }
        let mut addresses = BTreeMap::new();
        for (key, bytes) in &values {
            validate_state_key(key)?;
            validate_state_value(bytes)?;
            addresses.insert(
                key.clone(),
                object_address(RuntimeObjectKind::NamedStateValue, bytes)
                    .map_err(StateError::Address)?,
            );
        }
        Ok(Self { values, addresses })
    }

    pub(crate) const fn addresses(&self) -> &BTreeMap<String, RuntimeObjectAddress> {
        &self.addresses
    }

    pub(crate) fn integrity_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (key, address) in &self.addresses {
            bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
            bytes.extend_from_slice(key.as_bytes());
            bytes.push(address.kind() as u8);
            bytes.extend_from_slice(&address.codec_revision().to_be_bytes());
            bytes.extend_from_slice(address.digest().bytes());
        }
        bytes
    }
}

pub(crate) fn validate_state_key(key: &str) -> Result<(), StateError> {
    let mut bytes = key.bytes();
    if key.len() > 64
        || !bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(StateError::InvalidKey);
    }
    Ok(())
}

pub(crate) fn decode_state_value(bytes: &[u8]) -> Result<Value, StateError> {
    if bytes.len() > MAX_STATE_VALUE_BYTES {
        return Err(StateError::ValueTooLarge { bytes: bytes.len() });
    }
    serde_json::from_slice(bytes).map_err(StateError::from)
}

pub(crate) fn validate_state_value(bytes: &[u8]) -> Result<(), StateError> {
    let value = decode_state_value(bytes)?;
    validate_numbers(&value)?;
    if serde_json_canonicalizer::to_vec(&value).map_err(StateError::from)? != bytes {
        return Err(StateError::NonCanonical);
    }
    Ok(())
}

fn validate_numbers(value: &Value) -> Result<(), StateError> {
    const MAX_EXACT: u64 = (1 << 53) - 1;
    match value {
        Value::Number(number)
            if number
                .as_i64()
                .is_some_and(|n| n.unsigned_abs() > MAX_EXACT)
                || number.as_u64().is_some_and(|n| n > MAX_EXACT)
                || number
                    .as_f64()
                    .is_some_and(|n| n.fract() == 0.0 && n.abs() >= 2.0_f64.powi(53)) =>
        {
            Err(StateError::InexactNumber)
        }
        Value::Array(values) => values.iter().try_for_each(validate_numbers),
        Value::Object(values) => values.values().try_for_each(validate_numbers),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
