//! Write-only secret text: an API key, a pasted authorization code, an
//! authorize URL or user code. Debug output is redacted and the buffer is
//! zeroized on drop.

use std::fmt;

use zeroize::Zeroizing;

/// Bounded secret text carried by an admin call.
#[derive(Eq, PartialEq)]
pub struct SecretText(Zeroizing<String>);

impl SecretText {
    /// The most bytes one secret holds, whitespace included.
    pub const MAX_BYTES: usize = 16 * 1024;

    /// Takes ownership of the text.
    ///
    /// # Errors
    /// Refuses text over [`Self::MAX_BYTES`] without reflecting it.
    pub fn new(value: Zeroizing<String>) -> Result<Self, SecretTextTooLarge> {
        if value.len() > Self::MAX_BYTES {
            return Err(SecretTextTooLarge);
        }
        Ok(Self(value))
    }

    /// The secret text, for the one consumer that needs it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// A secret over [`SecretText::MAX_BYTES`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretTextTooLarge;

impl fmt::Display for SecretTextTooLarge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("secret text exceeds its byte limit")
    }
}

impl std::error::Error for SecretTextTooLarge {}

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretText(<redacted>)")
    }
}

impl serde::Serialize for SecretText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}

impl<'de> serde::Deserialize<'de> for SecretText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = SecretText;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded secret string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.len() > SecretText::MAX_BYTES {
                    return Err(E::custom(SecretTextTooLarge));
                }
                SecretText::new(Zeroizing::new(value.to_owned())).map_err(E::custom)
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                SecretText::new(Zeroizing::new(value)).map_err(E::custom)
            }
        }

        deserializer.deserialize_string(Visitor)
    }
}
