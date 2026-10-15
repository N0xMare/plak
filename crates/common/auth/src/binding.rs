//! OAuth PKCE and callback-correlation primitives.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use zeroize::Zeroize as _;

use crate::SecretString;

const RANDOM_TOKEN_BYTES: usize = 32;
const GENERATED_TOKEN_LENGTH: usize = 43;
const MIN_PKCE_VERIFIER_LENGTH: usize = 43;
const MAX_PKCE_VERIFIER_LENGTH: usize = 128;
const MIN_CALLBACK_VALUE_LENGTH: usize = 32;
const MAX_CALLBACK_VALUE_LENGTH: usize = 128;

/// A validated, non-cloneable PKCE code verifier.
pub struct PkceVerifier(SecretString);

impl PkceVerifier {
    /// Generates a verifier from 256 bits supplied by the operating system.
    ///
    /// # Errors
    ///
    /// Returns [`BindingError::RandomnessUnavailable`] when secure operating
    /// system randomness is unavailable.
    pub fn generate() -> Result<Self, BindingError> {
        Ok(Self(SecretString::from(random_token()?)))
    }

    /// Validates an RFC 7636 verifier without cloning it.
    ///
    /// # Errors
    ///
    /// Rejects values outside the RFC length and character bounds.
    pub fn new(verifier: SecretString) -> Result<Self, BindingError> {
        if !valid_token(
            verifier.expose_secret(),
            MIN_PKCE_VERIFIER_LENGTH,
            MAX_PKCE_VERIFIER_LENGTH,
        ) {
            return Err(BindingError::InvalidPkceVerifier);
        }
        Ok(Self(verifier))
    }

    /// Borrows the verifier for an OAuth token request.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }

    /// Computes the RFC 7636 S256 challenge.
    #[must_use]
    pub fn challenge(&self) -> PkceChallenge {
        let digest = Sha256::digest(self.0.expose_secret().as_bytes());
        PkceChallenge(URL_SAFE_NO_PAD.encode(digest))
    }
}

impl fmt::Debug for PkceVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PkceVerifier([REDACTED])")
    }
}

/// A public PKCE S256 code challenge.
#[derive(Clone, Eq, PartialEq)]
pub struct PkceChallenge(String);

impl PkceChallenge {
    /// Returns the URL-safe unpadded S256 challenge.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PkceChallenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PkceChallenge")
            .field(&self.0)
            .finish()
    }
}

/// A generated PKCE S256 verifier/challenge pair.
pub struct PkceS256 {
    verifier: PkceVerifier,
    challenge: PkceChallenge,
}

impl PkceS256 {
    /// Generates a fresh S256 pair.
    ///
    /// # Errors
    ///
    /// Returns [`BindingError::RandomnessUnavailable`] when secure operating
    /// system randomness is unavailable.
    pub fn generate() -> Result<Self, BindingError> {
        let verifier = PkceVerifier::generate()?;
        let challenge = verifier.challenge();
        Ok(Self {
            verifier,
            challenge,
        })
    }

    /// Builds a pair from a validated verifier.
    #[must_use]
    pub fn from_verifier(verifier: PkceVerifier) -> Self {
        let challenge = verifier.challenge();
        Self {
            verifier,
            challenge,
        }
    }

    /// Returns the non-cloneable verifier.
    #[must_use]
    pub const fn verifier(&self) -> &PkceVerifier {
        &self.verifier
    }

    /// Returns the public S256 challenge.
    #[must_use]
    pub const fn challenge(&self) -> &PkceChallenge {
        &self.challenge
    }
}

impl fmt::Debug for PkceS256 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PkceS256")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// Cryptographic OAuth state and OIDC nonce correlation values.
///
/// Both values are non-cloneable and verified in constant time over fixed
/// bounded buffers.
pub struct CallbackBinding {
    state: SecretString,
    nonce: SecretString,
}

impl CallbackBinding {
    /// Generates independent 256-bit state and nonce values.
    ///
    /// # Errors
    ///
    /// Returns [`BindingError::RandomnessUnavailable`] when secure operating
    /// system randomness is unavailable.
    pub fn generate() -> Result<Self, BindingError> {
        Ok(Self {
            state: SecretString::from(random_token()?),
            nonce: SecretString::from(random_token()?),
        })
    }

    /// Validates caller-supplied state and nonce values without cloning them.
    ///
    /// This is primarily useful when a higher layer supplies its own
    /// cryptographically random values.
    ///
    /// # Errors
    ///
    /// Rejects values outside the bounded URL-safe token grammar.
    pub fn new(state: SecretString, nonce: SecretString) -> Result<Self, BindingError> {
        if !valid_token(
            state.expose_secret(),
            MIN_CALLBACK_VALUE_LENGTH,
            MAX_CALLBACK_VALUE_LENGTH,
        ) || !valid_token(
            nonce.expose_secret(),
            MIN_CALLBACK_VALUE_LENGTH,
            MAX_CALLBACK_VALUE_LENGTH,
        ) {
            return Err(BindingError::InvalidCallbackBinding);
        }
        Ok(Self { state, nonce })
    }

    /// Borrows state for the authorization request.
    #[must_use]
    pub fn expose_state(&self) -> &str {
        self.state.expose_secret()
    }

    /// Borrows nonce for the authorization request.
    #[must_use]
    pub fn expose_nonce(&self) -> &str {
        self.nonce.expose_secret()
    }

    /// Verifies both returned values in constant time over fixed buffers.
    ///
    /// # Errors
    ///
    /// Returns [`BindingError::CallbackMismatch`] without identifying which
    /// correlation value failed.
    pub fn verify(&self, returned_state: &str, returned_nonce: &str) -> Result<(), BindingError> {
        let state_matches = constant_time_bounded_eq(
            self.state.expose_secret().as_bytes(),
            returned_state.as_bytes(),
        );
        let nonce_matches = constant_time_bounded_eq(
            self.nonce.expose_secret().as_bytes(),
            returned_nonce.as_bytes(),
        );
        if state_matches & nonce_matches {
            Ok(())
        } else {
            Err(BindingError::CallbackMismatch)
        }
    }
}

impl fmt::Debug for CallbackBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallbackBinding")
            .field("state", &"<redacted>")
            .field("nonce", &"<redacted>")
            .finish()
    }
}

/// PKCE or callback-correlation failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum BindingError {
    /// Secure random bytes could not be obtained.
    #[error("secure operating system randomness is unavailable")]
    RandomnessUnavailable,
    /// The verifier violates RFC 7636 bounds or grammar.
    #[error("invalid PKCE verifier")]
    InvalidPkceVerifier,
    /// State or nonce violates the fixed callback bounds or grammar.
    #[error("invalid callback binding")]
    InvalidCallbackBinding,
    /// Returned state or nonce did not match.
    #[error("OAuth callback binding mismatch")]
    CallbackMismatch,
}

/// Generates one URL-safe OAuth correlation token (for bare `state` dialects).
///
/// # Errors
///
/// [`BindingError::RandomnessUnavailable`] when OS randomness fails.
pub fn generate_oauth_correlation_token() -> Result<SecretString, BindingError> {
    Ok(SecretString::from(random_token()?))
}

/// Mints one login selection epoch: 32 random bytes, base64url without padding.
///
/// Every successful subscription login stores a fresh one beside its tokens and
/// refresh carries it unchanged, so a refreshed generation is told apart from a
/// relogin of the same account. It is random but not secret.
///
/// # Errors
///
/// [`BindingError::RandomnessUnavailable`] when OS randomness fails.
pub fn generate_login_selection_epoch() -> Result<String, BindingError> {
    random_token()
}

/// Whether `value` has the exact shape [`generate_login_selection_epoch`] mints.
#[must_use]
pub fn valid_login_selection_epoch(value: &str) -> bool {
    value.len() == GENERATED_TOKEN_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn random_token() -> Result<String, BindingError> {
    let mut bytes = [0_u8; RANDOM_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| BindingError::RandomnessUnavailable)?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    debug_assert_eq!(token.len(), GENERATED_TOKEN_LENGTH);
    Ok(token)
}

fn valid_token(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

fn constant_time_bounded_eq(expected: &[u8], returned: &[u8]) -> bool {
    let mut expected_padded = [0_u8; MAX_CALLBACK_VALUE_LENGTH];
    let mut returned_padded = [0_u8; MAX_CALLBACK_VALUE_LENGTH];
    let expected_bounded = expected.len() <= MAX_CALLBACK_VALUE_LENGTH;
    let returned_bounded = returned.len() <= MAX_CALLBACK_VALUE_LENGTH;
    if expected_bounded {
        expected_padded[..expected.len()].copy_from_slice(expected);
    }
    if returned_bounded {
        returned_padded[..returned.len()].copy_from_slice(returned);
    }
    let bytes_equal = expected_padded.ct_eq(&returned_padded);
    let lengths_equal = expected
        .len()
        .to_le_bytes()
        .ct_eq(&returned.len().to_le_bytes());
    let result =
        expected_bounded & returned_bounded & bool::from(bytes_equal) & bool::from(lengths_equal);
    expected_padded.zeroize();
    returned_padded.zeroize();
    result
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    #[test]
    fn login_selection_epoch_has_one_encoding() {
        let first = generate_login_selection_epoch().unwrap();
        let second = generate_login_selection_epoch().unwrap();
        assert_ne!(first, second);
        assert!(valid_login_selection_epoch(&first));
        let rejected = [
            String::new(),
            "0123456789abcdef".repeat(4),
            first[1..].to_owned(),
            format!("{first}A"),
            format!("{}=", &first[1..]),
            format!("{}.", &first[1..]),
        ];
        for value in &rejected {
            assert!(!valid_login_selection_epoch(value), "{value}");
        }
    }

    use super::*;

    #[test]
    fn rfc_7636_s256_vector() -> Result<(), BindingError> {
        let verifier = PkceVerifier::new(SecretString::from(String::from(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        )))?;
        assert_eq!(
            verifier.challenge().as_str(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        Ok(())
    }

    #[test]
    fn generated_values_are_bounded_and_distinct() -> Result<(), BindingError> {
        let pkce = PkceS256::generate()?;
        let callback = CallbackBinding::generate()?;
        assert_eq!(
            pkce.verifier().expose_secret().len(),
            GENERATED_TOKEN_LENGTH
        );
        assert_eq!(pkce.challenge().as_str().len(), GENERATED_TOKEN_LENGTH);
        assert_eq!(callback.expose_state().len(), GENERATED_TOKEN_LENGTH);
        assert_eq!(callback.expose_nonce().len(), GENERATED_TOKEN_LENGTH);
        assert_ne!(callback.expose_state(), callback.expose_nonce());
        Ok(())
    }

    #[test]
    fn callback_requires_both_values() -> Result<(), BindingError> {
        let state = String::from("01234567890123456789012345678901state");
        let nonce = String::from("01234567890123456789012345678901nonce");
        let binding = CallbackBinding::new(
            SecretString::from(state.clone()),
            SecretString::from(nonce.clone()),
        )?;
        assert_eq!(binding.verify(&state, &nonce), Ok(()));
        assert_eq!(
            binding.verify(&format!("{state}x"), &nonce),
            Err(BindingError::CallbackMismatch)
        );
        assert_eq!(
            binding.verify(&state, &"x".repeat(10_000)),
            Err(BindingError::CallbackMismatch)
        );
        assert!(!format!("{binding:?}").contains(&state));
        Ok(())
    }
}
