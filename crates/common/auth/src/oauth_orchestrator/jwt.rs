//! Content-free signed identity rejection classification.

use super::CredentialRefreshFailureReason;

/// Verifies a signed identity using the process's JWT provider.
///
/// Installs the verification-only ring provider before the first decode unless
/// an embedder already called `CryptoProvider::install_default`. An embedder
/// needing another provider must install it before the first Plakous identity
/// verification; later installation and crate-feature defaults do not replace it.
///
/// # Errors
///
/// Returns the JWT signature, algorithm, key or claim validation failure.
#[allow(
    clippy::disallowed_methods,
    reason = "the one JWT decode boundary installs the process provider before verifying"
)]
pub fn decode<T: serde::de::DeserializeOwned>(
    token: &str,
    key: &jsonwebtoken::DecodingKey,
    validation: &jsonwebtoken::Validation,
) -> jsonwebtoken::errors::Result<jsonwebtoken::TokenData<T>> {
    crate::jwt_crypto::install();
    jsonwebtoken::decode(token, key, validation)
}

/// Names the JWT check that rejected an ID token, without exposing its contents.
#[must_use]
pub fn jwt_failure_reason(error: &jsonwebtoken::errors::Error) -> CredentialRefreshFailureReason {
    use jsonwebtoken::errors::ErrorKind as K;
    match error.kind() {
        K::InvalidSignature => CredentialRefreshFailureReason::IdTokenSignature,
        K::ExpiredSignature => CredentialRefreshFailureReason::IdTokenExp,
        K::ImmatureSignature => CredentialRefreshFailureReason::IdTokenNbf,
        K::InvalidIssuer => CredentialRefreshFailureReason::IdTokenIss,
        K::InvalidAudience => CredentialRefreshFailureReason::IdTokenAud,
        K::InvalidSubject => CredentialRefreshFailureReason::IdTokenSub,
        K::MissingRequiredClaim(claim) if claim == "sub" => {
            CredentialRefreshFailureReason::IdTokenSub
        }
        K::MissingRequiredClaim(claim) if claim == "exp" => {
            CredentialRefreshFailureReason::IdTokenExp
        }
        K::InvalidAlgorithm => CredentialRefreshFailureReason::IdTokenHeader,
        K::InvalidToken | K::Base64(_) | K::Utf8(_) | K::Json(_) => {
            CredentialRefreshFailureReason::TokenShape
        }
        _ => CredentialRefreshFailureReason::IdTokenClaims,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::errors::{Error, ErrorKind};

    #[test]
    fn claims_have_distinct_content_free_failure_reasons() {
        for (kind, expected) in [
            (
                ErrorKind::InvalidSignature,
                CredentialRefreshFailureReason::IdTokenSignature,
            ),
            (
                ErrorKind::MissingRequiredClaim("exp".into()),
                CredentialRefreshFailureReason::IdTokenExp,
            ),
            (
                ErrorKind::MissingRequiredClaim("sub".into()),
                CredentialRefreshFailureReason::IdTokenSub,
            ),
            (
                ErrorKind::MissingRequiredClaim("other".into()),
                CredentialRefreshFailureReason::IdTokenClaims,
            ),
            (
                ErrorKind::InvalidToken,
                CredentialRefreshFailureReason::TokenShape,
            ),
        ] {
            assert_eq!(jwt_failure_reason(&Error::from(kind)), expected);
        }
    }
}
