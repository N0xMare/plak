//! Zeroizing secret containers backed by the production `secrecy` + `zeroize` crates.
//!
//! Public types intentionally **do not implement [`Clone`]**: call sites
//! must pass ownership rather than proliferating secret material. Internals use
//! [`secrecy::SecretString`] / [`secrecy::SecretBox`] for zeroization-on-drop.

use std::fmt;

use secrecy::SecretBox;

/// Re-export for callers that need the `ExposeSecret` trait at audit boundaries.
pub use secrecy::ExposeSecret;

/// An owned UTF-8 secret that is zeroized when dropped.
///
/// This type deliberately implements neither [`Clone`] nor Serde.
pub struct SecretString(secrecy::SecretString);

impl SecretString {
    /// Takes ownership of a secret string.
    #[must_use]
    pub fn new(secret: String) -> Self {
        Self(secrecy::SecretString::from(secret))
    }

    /// Borrows the secret for the shortest practical scope.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        ExposeSecret::expose_secret(&self.0)
    }

    /// Moves the secret into a byte container.
    ///
    /// Copies UTF-8 bytes into a new allocation; the string is zeroized on drop.
    #[must_use]
    pub fn into_secret_bytes(self) -> SecretBytes {
        SecretBytes::new(self.expose_secret().as_bytes().to_vec())
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

/// Owned secret bytes that are zeroized when dropped.
///
/// This type deliberately implements neither [`Clone`] nor Serde.
pub struct SecretBytes(SecretBox<Vec<u8>>);

impl SecretBytes {
    /// Takes ownership of secret bytes.
    #[must_use]
    pub fn new(secret: Vec<u8>) -> Self {
        Self(SecretBox::new(Box::new(secret)))
    }

    /// Returns an empty secret value.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Borrows the secret for the shortest practical scope.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        ExposeSecret::expose_secret(&self.0).as_slice()
    }

    /// Returns the secret length without exposing its contents.
    #[must_use]
    pub fn len(&self) -> usize {
        ExposeSecret::expose_secret(&self.0).len()
    }

    /// Returns whether this container has no secret bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        ExposeSecret::expose_secret(&self.0).is_empty()
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretBytes([REDACTED])")
    }
}

/// Converts a [`SecretString`] into zeroizing secret bytes.
#[must_use]
pub fn secret_string_into_bytes(secret: SecretString) -> SecretBytes {
    secret.into_secret_bytes()
}

/// Extension helpers for call-site ergonomics around [`SecretString`].
pub trait SecretStringExt {
    /// Moves the secret into a byte container.
    #[must_use]
    fn into_secret_bytes(self) -> SecretBytes;
}

impl SecretStringExt for SecretString {
    fn into_secret_bytes(self) -> SecretBytes {
        SecretString::into_secret_bytes(self)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{SecretBytes, SecretString, SecretStringExt};
    use std::marker::PhantomData;

    #[test]
    fn debug_is_redacted() {
        let string = SecretString::new(String::from("a unique string secret"));
        let bytes = SecretBytes::new(b"unique byte secret".to_vec());

        let string_debug = format!("{string:?}");
        let bytes_debug = format!("{bytes:?}");
        assert!(string_debug.contains("REDACTED"));
        assert!(bytes_debug.contains("REDACTED"));
        assert!(!string_debug.contains("unique"));
        assert!(!bytes_debug.contains("unique"));
    }

    #[test]
    fn secret_string_into_bytes_preserves_utf8() {
        let string = SecretString::from("byte-roundtrip");
        let bytes = SecretStringExt::into_secret_bytes(string);
        assert_eq!(bytes.expose_secret(), b"byte-roundtrip");
    }

    /// A secret must not be `Clone`: copying one is how a credential ends up in
    /// a log, a retry buffer or a second owner that outlives the zeroize.
    ///
    /// The old body only took a size and called an unconstrained function, so
    /// it stayed green no matter what `SecretString` implemented. Autoref
    /// specialization gives a real answer: `Probe<T>` by value resolves to the
    /// `T: Clone` impl when there is one, and otherwise autorefs to the
    /// blanket impl.
    #[test]
    #[allow(
        clippy::needless_borrow,
        reason = "the explicit borrow is the probe: the Clone impl is reached at one autoref and the fallback at two, so removing it changes which trait answers"
    )]
    fn secret_string_is_not_clone() {
        struct Probe<T>(PhantomData<T>);

        trait NotClone {
            fn is_clone(&self) -> bool {
                false
            }
        }
        impl<T> NotClone for &Probe<T> {}

        trait IsClone {
            fn is_clone(&self) -> bool {
                true
            }
        }
        impl<T: Clone> IsClone for Probe<T> {}

        assert!(
            (&Probe::<String>(PhantomData)).is_clone(),
            "the probe must see a Clone type as Clone"
        );
        assert!(
            !(&Probe::<SecretString>(PhantomData)).is_clone(),
            "SecretString must not be Clone"
        );
        assert!(
            !(&Probe::<SecretBytes>(PhantomData)).is_clone(),
            "SecretBytes must not be Clone"
        );
    }
}
