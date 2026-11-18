//! Wire strings whose length and shape are checked when they are built or decoded.

use std::fmt;

/// Why a wire string was refused. It never echoes the refused value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextError(&'static str);

impl TextError {
    pub(crate) const fn new(reason: &'static str) -> Self {
        Self(reason)
    }

    /// The fixed reason, safe to log.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for TextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for TextError {}

/// Text of at most `MAX` bytes with no control characters.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BoundedText<const MAX: usize>(String);

impl<const MAX: usize> BoundedText<MAX> {
    /// Checks the length and refuses control characters.
    ///
    /// # Errors
    /// Refuses text over `MAX` bytes or containing a control character.
    pub fn new(value: impl Into<String>) -> Result<Self, TextError> {
        let value = value.into();
        if value.len() > MAX {
            return Err(TextError::new("text exceeds its byte limit"));
        }
        if value.chars().any(char::is_control) {
            return Err(TextError::new("text contains a control character"));
        }
        Ok(Self(value))
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const MAX: usize> TryFrom<String> for BoundedText<MAX> {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<const MAX: usize> From<BoundedText<MAX>> for String {
    fn from(value: BoundedText<MAX>) -> Self {
        value.0
    }
}

/// A SHA-256 digest as 64 lowercase hexadecimal characters. It is a
/// non-secret binding, never a hash of a credential.
#[derive(
    Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct HexDigest(String);

impl HexDigest {
    /// Checks the exact shape.
    ///
    /// # Errors
    /// Refuses anything but 64 lowercase hexadecimal characters.
    pub fn new(value: impl Into<String>) -> Result<Self, TextError> {
        let value = value.into();
        if value.len() == 64 && value.bytes().all(is_lower_hex) {
            Ok(Self(value))
        } else {
            Err(TextError::new(
                "digest is not 64 lowercase hexadecimal characters",
            ))
        }
    }

    /// Encodes 32 digest bytes.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        let mut text = String::with_capacity(64);
        for byte in bytes {
            text.push(char::from(HEX[usize::from(byte >> 4)]));
            text.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(text)
    }

    /// The hexadecimal text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HexDigest {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<HexDigest> for String {
    fn from(value: HexDigest) -> Self {
        value.0
    }
}

/// A UUID in its canonical lowercase hyphenated form, such as an operation,
/// gateway instance or login attempt id.
#[derive(
    Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct UuidText(String);

impl UuidText {
    /// Checks the canonical shape; the version bits are not interpreted.
    ///
    /// # Errors
    /// Refuses anything but `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` in
    /// lowercase hexadecimal.
    pub fn new(value: impl Into<String>) -> Result<Self, TextError> {
        let value = value.into();
        let canonical = value.len() == 36
            && value.bytes().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => is_lower_hex(byte),
            });
        if canonical {
            Ok(Self(value))
        } else {
            Err(TextError::new("id is not a canonical lowercase UUID"))
        }
    }

    /// The UUID text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for UuidText {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<UuidText> for String {
    fn from(value: UuidText) -> Self {
        value.0
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

const fn is_lower_hex(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f')
}

/// One identifier segment: `[a-z][a-z0-9-]{0,31}`.
pub(crate) fn is_segment(value: &str) -> bool {
    let bytes = value.as_bytes();
    matches!(bytes.first(), Some(b'a'..=b'z'))
        && bytes.len() <= 32
        && bytes
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}
