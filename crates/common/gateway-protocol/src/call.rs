//! JSON call bodies: facts, refresh and admin requests and replies, the
//! versions reply and refusal bodies. Each is one strict JSON value of at
//! most [`MAX_CALL_BYTES`].

use std::fmt;

/// The largest JSON call body.
pub const MAX_CALL_BYTES: usize = 64 * 1024;
/// The most accounts one facts request names.
pub const MAX_FACTS_ACCOUNTS: usize = 32;

/// Why a call body was refused. It never echoes the body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallError {
    /// The body exceeds [`MAX_CALL_BYTES`].
    TooLarge,
    /// The body is not strict JSON of the expected shape.
    Json,
    /// The value could not be encoded.
    Encode,
}

impl fmt::Display for CallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooLarge => "gateway call body exceeds its size limit",
            Self::Json => "gateway call body is malformed",
            Self::Encode => "gateway call body could not be encoded",
        })
    }
}

impl std::error::Error for CallError {}

/// Encodes one call body.
///
/// # Errors
/// Refuses a value JSON cannot encode and a body over [`MAX_CALL_BYTES`].
pub fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, CallError> {
    let body = serde_json::to_vec(value).map_err(|_| CallError::Encode)?;
    if body.len() > MAX_CALL_BYTES {
        return Err(CallError::TooLarge);
    }
    Ok(body)
}

/// Decodes one call body.
///
/// # Errors
/// Refuses a body over [`MAX_CALL_BYTES`] and one that is not a valid `T`.
pub fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, CallError> {
    if body.len() > MAX_CALL_BYTES {
        return Err(CallError::TooLarge);
    }
    serde_json::from_slice(body).map_err(|_| CallError::Json)
}
