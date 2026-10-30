//! Pure authorization URL construction from subscription OAuth pins.

use url::Url;
use zeroize::Zeroizing;

use crate::oauth_pins::SubscriptionOAuthPins;

use super::OAuthWorkflowError;

/// Inputs for a PKCE authorization-code authorize URL.
///
/// Dialect-specific query pairs (for example `originator`, `plan`, `referrer`)
/// belong in [`Self::extra_pairs`]. Secrets other than correlation material must
/// not appear here; state may be sensitive and the resulting URL is zeroized on
/// drop.
#[derive(Clone, Copy, Debug)]
pub struct AuthorizeUrlRequest<'a> {
    /// Dialect pin table providing client id, default origin, path, and scopes.
    pub pins: SubscriptionOAuthPins,
    /// Optional override of [`SubscriptionOAuthPins::auth_origin`].
    ///
    /// Managers pass their resolved endpoint origin so test doubles and custom
    /// profiles stay authoritative. When `None`, the pin-table origin is used.
    pub auth_origin: Option<&'a str>,
    /// Exact redirect URI registered for this login attempt.
    pub redirect_uri: &'a str,
    /// Public PKCE S256 code challenge.
    pub code_challenge: &'a str,
    /// Opaque OAuth `state` correlation value.
    pub state: &'a str,
    /// Optional OIDC `nonce` when the dialect includes it on the authorize URL.
    pub nonce: Option<&'a str>,
    /// Additional dialect-owned query pairs, appended after standard fields.
    pub extra_pairs: &'a [(&'a str, &'a str)],
}

/// Builds a PKCE authorization URL from pins and attempt material.
///
/// Standard query fields, in order:
/// `response_type`, `client_id`, `redirect_uri`, `scope`, `code_challenge`,
/// `code_challenge_method=S256`, `state`, optional `nonce`, then `extra_pairs`.
///
/// # Errors
///
/// Returns [`OAuthWorkflowError::InvalidUrl`] when the pin origin or path cannot
/// form an absolute HTTP(S) URL, or when required string inputs are empty.
pub fn build_authorization_url(
    request: AuthorizeUrlRequest<'_>,
) -> Result<Zeroizing<String>, OAuthWorkflowError> {
    if request.redirect_uri.is_empty()
        || request.code_challenge.is_empty()
        || request.state.is_empty()
        || request.pins.client_id.is_empty()
        || request.pins.scopes.is_empty()
        || request.pins.authorize_path.is_empty()
        || request.nonce.is_some_and(str::is_empty)
        || request.auth_origin.is_some_and(str::is_empty)
    {
        return Err(OAuthWorkflowError::InvalidUrl);
    }

    let origin = request.auth_origin.unwrap_or(request.pins.auth_origin);
    let mut url = Url::parse(origin).map_err(|_| OAuthWorkflowError::InvalidUrl)?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(OAuthWorkflowError::InvalidUrl);
    }
    url.set_path(request.pins.authorize_path);
    url.set_query(None);
    url.set_fragment(None);

    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("response_type", "code")
            .append_pair("client_id", request.pins.client_id)
            .append_pair("redirect_uri", request.redirect_uri)
            .append_pair("scope", request.pins.scopes)
            .append_pair("code_challenge", request.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", request.state);
        if let Some(nonce) = request.nonce {
            pairs.append_pair("nonce", nonce);
        }
        for (name, value) in request.extra_pairs {
            if name.is_empty() {
                return Err(OAuthWorkflowError::InvalidUrl);
            }
            pairs.append_pair(name, value);
        }
    }

    Ok(Zeroizing::new(url.into()))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
