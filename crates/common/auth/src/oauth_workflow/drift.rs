//! Pure comparisons against compiled subscription contracts; no discovery adoption.

use crate::{CompatibilityDrift, oauth_pins::SubscriptionOAuthPins};
use serde_json::Value;

/// Checks only the scopes the caller actually requires, not its full request.
/// An omitted scope means the requested scope under RFC 6749 §5.1.
///
/// # Errors
///
/// Rejects a grant missing a required scope; ordering and extra grants are irrelevant.
pub fn validate_granted_scope(
    scope: Option<&str>,
    required: &str,
) -> Result<(), CompatibilityDrift> {
    let Some(scope) = scope else {
        return Ok(());
    };
    if required.split_ascii_whitespace().any(|required| {
        !scope
            .split_ascii_whitespace()
            .any(|served| served == required)
    }) {
        return Err(CompatibilityDrift::GrantedScope);
    }
    Ok(())
}

/// Compares OIDC metadata with exact compiled endpoints and requested scopes.
/// The caller continues using only its compiled URLs after this succeeds.
///
/// # Errors
///
/// Names the mismatched field without retaining served URLs or response prose.
pub fn validate_subscription_oidc(
    pins: SubscriptionOAuthPins,
    metadata: &Value,
) -> Result<(), CompatibilityDrift> {
    let origin = pins.auth_origin.trim_end_matches('/');
    for (field, path) in [
        ("issuer", ""),
        ("authorization_endpoint", pins.authorize_path),
        ("token_endpoint", pins.token_path),
        (
            "device_authorization_endpoint",
            pins.device_authorization_path,
        ),
        ("revocation_endpoint", pins.revoke_path),
        ("userinfo_endpoint", pins.userinfo_path),
        ("jwks_uri", pins.jwks_path),
    ] {
        let expected = if field == "issuer" {
            pins.auth_origin.to_owned()
        } else {
            format!("{origin}{path}")
        };
        if metadata.get(field).and_then(Value::as_str) != Some(expected.as_str()) {
            return Err(CompatibilityDrift::Endpoint { field });
        }
    }
    if let Some(scopes) = metadata.get("scopes_supported") {
        let supported = scopes.as_array().ok_or(CompatibilityDrift::Endpoint {
            field: "scopes_supported",
        })?;
        if supported.iter().any(|scope| !scope.is_string())
            || pins
                .scopes
                .split_ascii_whitespace()
                .any(|scope| !supported.iter().any(|value| value.as_str() == Some(scope)))
        {
            return Err(CompatibilityDrift::Endpoint {
                field: "scopes_supported",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "Fail fast on deterministic OIDC metadata fixtures."
)]
mod tests;
