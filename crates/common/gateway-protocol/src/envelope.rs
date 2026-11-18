//! The relay request: one upstream exchange the caller asks the gateway to
//! make with a custodied credential.
//!
//! The request body is a `u32` big-endian envelope length, the envelope as
//! strict JSON, then the upstream body bytes unchanged:
//!
//! ```text
//! u32be envelope_len | RelayEnvelope JSON | upstream body
//! ```
//!
//! The gateway reads the whole request before it validates anything or
//! dials upstream, so a request that did not arrive whole was never sent.
//! The header list is the exact upstream order: credential fields appear in
//! it as custodied entries that the custodian fills at their own position.
//! Which origins, methods and paths a credential may reach, and which header
//! names it fills, belong to the credential's provider; this module checks
//! only the protocol shape.

use std::fmt;

use crate::account::GatewayAccount;
use crate::frame::MAX_DATA_BYTES;
use crate::header::RequestHeader;
use crate::text::{HexDigest, UuidText};

/// The largest envelope, excluding its length prefix.
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
/// The most request header entries, custodied ones included, one envelope carries.
pub const MAX_REQUEST_HEADERS: usize = 64;
/// The most encoded request header bytes (names and values) one envelope carries.
pub const MAX_REQUEST_HEADER_BYTES: usize = 32 * 1024;
/// The most custodied entries one request holds.
pub const MAX_CUSTODIED_HEADERS: usize = 4;
/// The longest upstream URL.
pub const MAX_URL_BYTES: usize = 8 * 1024;
/// The longest upstream method token.
pub const MAX_METHOD_BYTES: usize = 16;

/// Why a relay request was refused. It never echoes request content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeError {
    /// The request ended before its envelope did.
    Truncated,
    /// The envelope exceeds [`MAX_ENVELOPE_BYTES`].
    EnvelopeTooLarge,
    /// The envelope is not strict JSON of the expected shape.
    Json,
    /// The envelope could not be encoded.
    Encode,
    /// The upstream body is not exactly `body_len` bytes.
    BodyLength,
    /// The method is not an uppercase token of at most [`MAX_METHOD_BYTES`].
    Method,
    /// The URL is not an absolute `https` URL of the accepted shape.
    Url,
    /// The request headers exceed their count or byte limits.
    HeaderLimits,
    /// A request header carries a name the caller may not send: a framing or
    /// hop-by-hop name, a gateway name, or the name of a custodied entry.
    ReservedHeader,
    /// The request has no custodied entry, more than
    /// [`MAX_CUSTODIED_HEADERS`], a repeated one, or one with a reserved name.
    CustodiedHeaders,
    /// A limit is zero or above what the protocol carries.
    Limits,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Truncated => "relay request ended inside its envelope",
            Self::EnvelopeTooLarge => "relay envelope exceeds its size limit",
            Self::Json => "relay envelope is malformed",
            Self::Encode => "relay envelope could not be encoded",
            Self::BodyLength => "relay body length does not match its envelope",
            Self::Method => "relay method is invalid",
            Self::Url => "relay upstream URL is invalid",
            Self::HeaderLimits => "relay request headers exceed their limits",
            Self::ReservedHeader => "relay request carries a reserved header",
            Self::CustodiedHeaders => "relay custodied header entries are invalid",
            Self::Limits => "relay limits are invalid",
        })
    }
}

impl std::error::Error for EnvelopeError {}

/// The credential state the caller prepared this request against. The
/// gateway refuses the request unless the custodied credential still has
/// exactly this account binding and generation.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedFence {
    /// Digest of the provider account identity.
    pub account_binding: HexDigest,
    /// The credential generation.
    pub generation: u64,
}

/// The caller's bounds for this exchange. The gateway applies the smaller of
/// these and its own maxima to its upstream exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayLimits {
    /// Wait for the upstream response head.
    pub head_timeout_ms: u64,
    /// Silence allowed between upstream body bytes.
    pub body_idle_timeout_ms: u64,
    /// Total upstream response body bytes.
    pub max_response_bytes: u64,
    /// Bytes in one data frame, at most [`MAX_DATA_BYTES`].
    pub max_chunk_bytes: u32,
}

/// One relay request's envelope.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayEnvelope {
    /// The caller's operation id: its ledger operation, or a fresh id for a
    /// catalog, usage or side read.
    pub operation: UuidText,
    /// The physical attempt within that operation.
    pub attempt: u32,
    /// The custodied account whose credential this request uses.
    pub account: GatewayAccount,
    /// The credential state the request was prepared against.
    pub expected: ExpectedFence,
    /// The upstream method.
    pub method: String,
    /// The absolute upstream URL, compared exactly with the credential's pins.
    pub url: String,
    /// The upstream request headers in exact order, duplicates preserved,
    /// with custodied entries where the credential's fields go.
    pub headers: Vec<RequestHeader>,
    /// Length of the upstream body that follows the envelope.
    pub body_len: u64,
    /// Bounds for the upstream exchange.
    pub limits: RelayLimits,
    /// The caller's hard deadline in Unix milliseconds, when it has one.
    pub deadline_unix_ms: Option<u64>,
}

impl RelayEnvelope {
    /// Checks the protocol shape. Provider pins are checked by the custodian.
    ///
    /// # Errors
    /// Refuses the first shape violation found.
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.method.is_empty()
            || self.method.len() > MAX_METHOD_BYTES
            || !self.method.bytes().all(|byte| byte.is_ascii_uppercase())
        {
            return Err(EnvelopeError::Method);
        }
        split_upstream_url(&self.url)?;
        let bytes: usize = self.headers.iter().map(RequestHeader::encoded_len).sum();
        if self.headers.len() > MAX_REQUEST_HEADERS || bytes > MAX_REQUEST_HEADER_BYTES {
            return Err(EnvelopeError::HeaderLimits);
        }
        self.validate_custodied()?;
        let custodied: Vec<&str> = self.custodied_names().collect();
        if self.headers.iter().any(|header| {
            matches!(header, RequestHeader::Value(value)
                if is_reserved_request_header(value.name()) || custodied.contains(&value.name()))
        }) {
            return Err(EnvelopeError::ReservedHeader);
        }
        let limits = &self.limits;
        if limits.head_timeout_ms == 0
            || limits.body_idle_timeout_ms == 0
            || limits.max_response_bytes == 0
            || limits.max_chunk_bytes == 0
            || usize::try_from(limits.max_chunk_bytes)
                .ok()
                .is_none_or(|bytes| bytes > MAX_DATA_BYTES)
        {
            return Err(EnvelopeError::Limits);
        }
        Ok(())
    }

    /// The names of the custodied entries, in header order.
    pub fn custodied_names(&self) -> impl Iterator<Item = &str> {
        self.headers.iter().filter_map(|header| match header {
            RequestHeader::Custodied { name } => Some(name.as_str()),
            RequestHeader::Value(_) => None,
        })
    }

    fn validate_custodied(&self) -> Result<(), EnvelopeError> {
        let names: Vec<&str> = self.custodied_names().collect();
        let valid = !names.is_empty()
            && names.len() <= MAX_CUSTODIED_HEADERS
            && names.iter().all(|name| !is_reserved_request_header(name))
            && names
                .iter()
                .enumerate()
                .all(|(index, name)| !names[..index].contains(name));
        if valid {
            Ok(())
        } else {
            Err(EnvelopeError::CustodiedHeaders)
        }
    }
}

/// Splits an absolute upstream URL into its origin (`https://host[:port]`)
/// and its path and query.
///
/// The host is lowercase; userinfo, fragments, whitespace and control
/// characters are refused, so the origin and path compare exactly with a
/// provider's pins.
///
/// # Errors
/// Refuses any other shape.
pub fn split_upstream_url(url: &str) -> Result<(&str, &str), EnvelopeError> {
    if url.len() > MAX_URL_BYTES {
        return Err(EnvelopeError::Url);
    }
    let rest = url.strip_prefix("https://").ok_or(EnvelopeError::Url)?;
    let split = rest.find('/').ok_or(EnvelopeError::Url)?;
    let (authority, path) = rest.split_at(split);
    let authority_ok = !authority.is_empty()
        && authority.bytes().all(
            |byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b':' | b'[' | b']'),
        );
    let path_ok = path
        .bytes()
        .all(|byte| (b'!'..=b'~').contains(&byte) && byte != b'#');
    if !authority_ok || !path_ok {
        return Err(EnvelopeError::Url);
    }
    Ok(url.split_at("https://".len() + split))
}

/// Whether a caller may never send this request header itself: framing and
/// hop-by-hop fields the gateway's own exchange owns, proxy fields, and the
/// gateway's own `plak-gateway-*` fields.
#[must_use]
pub fn is_reserved_request_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
    ) || name.starts_with("proxy-")
        || name.starts_with("plak-gateway-")
}

/// Encodes a relay request body: the envelope, then the upstream body.
///
/// # Errors
/// Refuses an invalid envelope, a body whose length differs from
/// `body_len`, and an envelope over [`MAX_ENVELOPE_BYTES`].
pub fn encode_request(envelope: &RelayEnvelope, body: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
    envelope.validate()?;
    if u64::try_from(body.len()).ok() != Some(envelope.body_len) {
        return Err(EnvelopeError::BodyLength);
    }
    let json = serde_json::to_vec(envelope).map_err(|_| EnvelopeError::Encode)?;
    if json.len() > MAX_ENVELOPE_BYTES {
        return Err(EnvelopeError::EnvelopeTooLarge);
    }
    let length = u32::try_from(json.len()).map_err(|_| EnvelopeError::EnvelopeTooLarge)?;
    let mut request = Vec::with_capacity(4 + json.len() + body.len());
    request.extend_from_slice(&length.to_be_bytes());
    request.extend_from_slice(&json);
    request.extend_from_slice(body);
    Ok(request)
}

/// Decodes a whole relay request body into its validated envelope and the
/// upstream body bytes.
///
/// # Errors
/// Refuses a truncated or oversized envelope, malformed or unknown fields,
/// an invalid envelope and a body of any length but `body_len`.
pub fn decode_request(request: &[u8]) -> Result<(RelayEnvelope, &[u8]), EnvelopeError> {
    let (prefix, rest) = request
        .split_first_chunk::<4>()
        .ok_or(EnvelopeError::Truncated)?;
    let length = usize::try_from(u32::from_be_bytes(*prefix))
        .map_err(|_| EnvelopeError::EnvelopeTooLarge)?;
    if length > MAX_ENVELOPE_BYTES {
        return Err(EnvelopeError::EnvelopeTooLarge);
    }
    if rest.len() < length {
        return Err(EnvelopeError::Truncated);
    }
    let (json, body) = rest.split_at(length);
    let envelope: RelayEnvelope = serde_json::from_slice(json).map_err(|_| EnvelopeError::Json)?;
    envelope.validate()?;
    if u64::try_from(body.len()).ok() != Some(envelope.body_len) {
        return Err(EnvelopeError::BodyLength);
    }
    Ok((envelope, body))
}
