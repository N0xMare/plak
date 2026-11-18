//! HTTP header fields as the protocol carries them: a lowercase name and the
//! exact value bytes, encoded as `[name, standard base64 value]`. A relay
//! request's list may also hold custodied entries, `[name, null]`, whose
//! value the custodian fills in place.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The longest header name the protocol carries.
pub const MAX_HEADER_NAME_BYTES: usize = 128;
/// The longest header value the protocol carries.
pub const MAX_HEADER_VALUE_BYTES: usize = 16 * 1024;

/// Why a header field was refused. It never echoes the field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderError {
    /// The name is empty, too long, or not a lowercase HTTP token.
    Name,
    /// The value is too long, is not standard base64, or holds a byte HTTP
    /// forbids in a field value (control characters other than tab).
    Value,
}

impl fmt::Display for HeaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "header name is not a lowercase HTTP token",
            Self::Value => "header value is not a valid HTTP field value",
        })
    }
}

impl std::error::Error for HeaderError {}

/// One header field. Order and duplicates are preserved by the lists that
/// carry it.
#[derive(Clone, Eq, PartialEq)]
pub struct WireHeader {
    name: String,
    value: Vec<u8>,
}

impl WireHeader {
    /// Checks the name and value.
    ///
    /// # Errors
    /// Refuses a name that is not a lowercase token or a value HTTP forbids.
    pub fn new(name: impl Into<String>, value: impl Into<Vec<u8>>) -> Result<Self, HeaderError> {
        let name = name.into();
        let value = value.into();
        if !is_header_name(&name) {
            return Err(HeaderError::Name);
        }
        if value.len() > MAX_HEADER_VALUE_BYTES
            || !value
                .iter()
                .all(|byte| *byte == b'\t' || (b' '..=b'~').contains(byte) || *byte >= 0x80)
        {
            return Err(HeaderError::Value);
        }
        Ok(Self { name, value })
    }

    /// The lowercase name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The exact value bytes.
    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// Bytes this field counts against a header byte limit: name and value.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.name.len() + self.value.len()
    }
}

/// Values may hold credentials or account identifiers, so only names print.
impl fmt::Debug for WireHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WireHeader")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl serde::Serialize for WireHeader {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (&self.name, STANDARD.encode(&self.value)).serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for WireHeader {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (name, encoded) = <(String, String)>::deserialize(deserializer)?;
        decode_value(name, &encoded).map_err(serde::de::Error::custom)
    }
}

/// One entry of a relay request's ordered header list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestHeader {
    /// A non-secret field sent as given.
    Value(WireHeader),
    /// A credential field: the custodian substitutes the credential's value
    /// at exactly this position. Encoded as `[name, null]`.
    Custodied {
        /// The lowercase field name.
        name: String,
    },
}

impl RequestHeader {
    /// A custodied entry.
    ///
    /// # Errors
    /// Refuses a name that is not a lowercase token.
    pub fn custodied(name: impl Into<String>) -> Result<Self, HeaderError> {
        let name = name.into();
        if is_header_name(&name) {
            Ok(Self::Custodied { name })
        } else {
            Err(HeaderError::Name)
        }
    }

    /// The field name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Value(header) => header.name(),
            Self::Custodied { name } => name,
        }
    }

    /// Bytes this entry counts against a header byte limit. A custodied entry
    /// counts its name only; the gateway bounds the value it fills.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Value(header) => header.encoded_len(),
            Self::Custodied { name } => name.len(),
        }
    }
}

impl serde::Serialize for RequestHeader {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Value(header) => header.serialize(serializer),
            Self::Custodied { name } => (name, Option::<&str>::None).serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for RequestHeader {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (name, encoded) = <(String, Option<String>)>::deserialize(deserializer)?;
        match encoded {
            None => Self::custodied(name).map_err(serde::de::Error::custom),
            Some(encoded) => decode_value(name, &encoded)
                .map(Self::Value)
                .map_err(serde::de::Error::custom),
        }
    }
}

fn decode_value(name: String, encoded: &str) -> Result<WireHeader, HeaderError> {
    if encoded.len() > MAX_HEADER_VALUE_BYTES.div_ceil(3) * 4 {
        return Err(HeaderError::Value);
    }
    let value = STANDARD.decode(encoded).map_err(|_| HeaderError::Value)?;
    WireHeader::new(name, value)
}

/// Whether `name` is a lowercase HTTP token of at most
/// [`MAX_HEADER_NAME_BYTES`] bytes.
#[must_use]
pub fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME_BYTES
        && name.bytes().all(|byte| {
            matches!(byte,
                b'a'..=b'z' | b'0'..=b'9'
                | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
                | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
        })
}

/// Checks a response header list against a field count and an encoded byte limit.
pub(crate) fn within_limits(headers: &[WireHeader], count: usize, bytes: usize) -> bool {
    headers.len() <= count && headers.iter().map(WireHeader::encoded_len).sum::<usize>() <= bytes
}
