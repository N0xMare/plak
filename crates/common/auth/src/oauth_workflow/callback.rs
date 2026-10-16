//! Pure OAuth callback query parsing helpers.

use std::collections::BTreeMap;

use zeroize::Zeroizing;

use super::OAuthWorkflowError;

/// Policy for accepting OAuth authorization callback query parameters.
///
/// Managers keep HTTP request framing and Host checks; this policy only shapes
/// the query string / absolute redirect query surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallbackQueryPolicy {
    /// Maximum accepted query string length in bytes.
    pub max_query_bytes: usize,
    /// Maximum number of distinct keys.
    pub max_keys: usize,
    /// Whether `error_uri` is an allowed parameter.
    pub allow_error_uri: bool,
    /// Maximum authorization `code` length.
    pub max_code_bytes: usize,
    /// Maximum `state` length.
    pub max_state_bytes: usize,
    /// Maximum `error` length.
    pub max_error_bytes: usize,
    /// Maximum `error_description` length.
    pub max_error_description_bytes: usize,
    /// Maximum `error_uri` length when allowed.
    pub max_error_uri_bytes: usize,
    /// When set, a present `scope` must equal this exact string.
    pub exact_scope: Option<&'static str>,
    /// When true, every raw field must contain `=` (no bare keys).
    pub require_equals_in_fields: bool,
}

impl CallbackQueryPolicy {
    /// Default policy matching common authorization-code callback bounds.
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            max_query_bytes: 8 * 1024,
            max_keys: 6,
            allow_error_uri: false,
            max_code_bytes: 16 * 1024,
            max_state_bytes: 128,
            max_error_bytes: 128,
            max_error_description_bytes: 1024,
            max_error_uri_bytes: 2048,
            exact_scope: None,
            require_equals_in_fields: false,
        }
    }

    /// Codex-style callback policy (no `error_uri`, max four keys, exact scopes).
    #[must_use]
    pub const fn codex(exact_scope: &'static str) -> Self {
        Self {
            max_query_bytes: 8 * 1024,
            max_keys: 4,
            allow_error_uri: false,
            max_code_bytes: 16 * 1024,
            max_state_bytes: 128,
            max_error_bytes: 128,
            max_error_description_bytes: 1024,
            max_error_uri_bytes: 2048,
            exact_scope: Some(exact_scope),
            require_equals_in_fields: false,
        }
    }

    /// SuperGrok-style callback policy (`error_uri` allowed, equals-required fields).
    #[must_use]
    pub const fn xai(exact_scope: &'static str) -> Self {
        Self {
            max_query_bytes: 8 * 1024,
            max_keys: 6,
            allow_error_uri: true,
            max_code_bytes: 4096,
            max_state_bytes: 128,
            max_error_bytes: 128,
            max_error_description_bytes: 1024,
            max_error_uri_bytes: 2048,
            exact_scope: Some(exact_scope),
            require_equals_in_fields: true,
        }
    }
}

/// Successful or provider-error outcome of a callback query parse.
#[derive(Debug)]
pub enum OAuthCallbackOutcome {
    /// Authorization code returned with correlated state.
    AuthorizationCode {
        /// Provider authorization code (zeroized on drop).
        code: Zeroizing<String>,
        /// OAuth state (zeroized on drop).
        state: Zeroizing<String>,
    },
    /// Provider reported an OAuth error with correlated state.
    ProviderError {
        /// OAuth state (zeroized on drop).
        state: Zeroizing<String>,
    },
}

impl PartialEq for OAuthCallbackOutcome {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::AuthorizationCode {
                    code: left_code,
                    state: left_state,
                },
                Self::AuthorizationCode {
                    code: right_code,
                    state: right_state,
                },
            ) => {
                left_code.as_str() == right_code.as_str()
                    && left_state.as_str() == right_state.as_str()
            }
            (
                Self::ProviderError { state: left_state },
                Self::ProviderError { state: right_state },
            ) => left_state.as_str() == right_state.as_str(),
            _ => false,
        }
    }
}

impl Eq for OAuthCallbackOutcome {}

/// Parses an OAuth callback query string under `policy`.
///
/// Accepts only known OAuth keys (`code`, `state`, `scope`, `error`,
/// `error_description`, and optionally `error_uri`). Duplicate keys, unknown
/// keys, mixed success/error, and bound violations fail closed.
///
/// # Errors
///
/// Returns [`OAuthWorkflowError::Callback`] for any policy or grammar failure.
pub fn parse_oauth_callback_query(
    query: &str,
    policy: CallbackQueryPolicy,
) -> Result<OAuthCallbackOutcome, OAuthWorkflowError> {
    if query.is_empty() || query.len() > policy.max_query_bytes || query.contains('#') {
        return Err(OAuthWorkflowError::Callback);
    }
    if policy.require_equals_in_fields
        && query
            .split('&')
            .any(|field| field.is_empty() || !field.contains('='))
    {
        return Err(OAuthWorkflowError::Callback);
    }

    let mut values: BTreeMap<String, Zeroizing<String>> = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let allowed = matches!(
            key.as_ref(),
            "code" | "state" | "scope" | "error" | "error_description"
        ) || (policy.allow_error_uri && key.as_ref() == "error_uri");
        if !allowed
            || values
                .insert(key.into_owned(), Zeroizing::new(value.into_owned()))
                .is_some()
            || values.len() > policy.max_keys
        {
            return Err(OAuthWorkflowError::Callback);
        }
    }

    let state = values.remove("state").ok_or(OAuthWorkflowError::Callback)?;
    if !valid_callback_value(state.as_str(), policy.max_state_bytes) {
        return Err(OAuthWorkflowError::Callback);
    }

    if let Some(error) = values.remove("error") {
        if !valid_callback_value(error.as_str(), policy.max_error_bytes) {
            return Err(OAuthWorkflowError::Callback);
        }
        if values.contains_key("code") || values.contains_key("scope") {
            return Err(OAuthWorkflowError::Callback);
        }
        if let Some(description) = values.remove("error_description")
            && !valid_callback_value(description.as_str(), policy.max_error_description_bytes)
        {
            return Err(OAuthWorkflowError::Callback);
        }
        if let Some(uri) = values.remove("error_uri")
            && (!policy.allow_error_uri
                || !valid_callback_value(uri.as_str(), policy.max_error_uri_bytes))
        {
            return Err(OAuthWorkflowError::Callback);
        }
        if !values.is_empty() {
            return Err(OAuthWorkflowError::Callback);
        }
        return Ok(OAuthCallbackOutcome::ProviderError { state });
    }

    if values.contains_key("error_description") || values.contains_key("error_uri") {
        return Err(OAuthWorkflowError::Callback);
    }

    let code = values.remove("code").ok_or(OAuthWorkflowError::Callback)?;
    if !valid_callback_value(code.as_str(), policy.max_code_bytes) {
        return Err(OAuthWorkflowError::Callback);
    }
    if let Some(scope) = values.remove("scope") {
        if let Some(exact) = policy.exact_scope {
            if scope.as_str() != exact {
                return Err(OAuthWorkflowError::Callback);
            }
        } else if scope.is_empty() {
            return Err(OAuthWorkflowError::Callback);
        }
    }
    if !values.is_empty() {
        return Err(OAuthWorkflowError::Callback);
    }
    Ok(OAuthCallbackOutcome::AuthorizationCode { code, state })
}

/// Parses the query of an absolute redirect URL that must match `expected_redirect_uri`.
///
/// Requires an exact `expected_redirect_uri?` prefix, printable ASCII, no
/// fragment, and a non-empty query accepted by [`parse_oauth_callback_query`].
///
/// # Errors
///
/// Returns [`OAuthWorkflowError::Callback`] when the absolute URL or query is
/// rejected.
pub fn parse_absolute_redirect_query(
    full_url: &str,
    expected_redirect_uri: &str,
    policy: CallbackQueryPolicy,
) -> Result<OAuthCallbackOutcome, OAuthWorkflowError> {
    if full_url.is_empty()
        || full_url.len() > policy.max_query_bytes + expected_redirect_uri.len() + 1
        || !full_url.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(OAuthWorkflowError::Callback);
    }
    let prefix = format!("{expected_redirect_uri}?");
    let query = full_url
        .strip_prefix(&prefix)
        .filter(|query| !query.is_empty())
        .ok_or(OAuthWorkflowError::Callback)?;
    if query.contains('#') {
        return Err(OAuthWorkflowError::Callback);
    }
    parse_oauth_callback_query(query, policy)
}

/// Returns the `host:port` authority of a plain-HTTP loopback redirect URI.
///
/// `None` unless `redirect_uri` is `http://<authority>/<path>` with a
/// non-empty authority.
#[must_use]
pub fn loopback_redirect_authority(redirect_uri: &str) -> Option<&str> {
    let (authority, _path) = redirect_uri.strip_prefix("http://")?.split_once('/')?;
    (!authority.is_empty()).then_some(authority)
}

/// Parses one loopback callback request against the attempt's redirect URI.
///
/// The `Host` header must equal the redirect's authority exactly, and
/// `http://<host><target>` must then pass [`parse_absolute_redirect_query`]
/// against that same redirect, so host, port, path and query all derive from
/// the redirect the attempt actually sent.
///
/// # Errors
///
/// Returns [`OAuthWorkflowError::Callback`] for a mismatched host or a
/// rejected redirect.
pub fn parse_loopback_callback_target(
    host: &str,
    target: &str,
    expected_redirect_uri: &str,
    policy: CallbackQueryPolicy,
) -> Result<OAuthCallbackOutcome, OAuthWorkflowError> {
    if loopback_redirect_authority(expected_redirect_uri) != Some(host) {
        return Err(OAuthWorkflowError::Callback);
    }
    let full_url = Zeroizing::new(format!("http://{host}{target}"));
    parse_absolute_redirect_query(&full_url, expected_redirect_uri, policy)
}

/// Parses an exact pasted redirect or binds a bare code to the live attempt.
///
/// # Errors
///
/// Returns a payload-free callback error for invalid input.
pub fn parse_remote_authorization_input(
    input: &str,
    attempt_state: &str,
    expected_redirect_uri: &str,
    policy: CallbackQueryPolicy,
) -> Result<OAuthCallbackOutcome, OAuthWorkflowError> {
    let trimmed = input.trim();
    if let Ok(callback) = parse_absolute_redirect_query(trimmed, expected_redirect_uri, policy) {
        return Ok(callback);
    }
    if !(16..=2_048).contains(&trimmed.len())
        || !trimmed.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/' | b'+' | b'=')
        })
    {
        return Err(OAuthWorkflowError::Callback);
    }
    Ok(OAuthCallbackOutcome::AuthorizationCode {
        code: Zeroizing::new(trimmed.to_owned()),
        state: Zeroizing::new(attempt_state.to_owned()),
    })
}

fn valid_callback_value(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{
        CallbackQueryPolicy, OAuthCallbackOutcome, loopback_redirect_authority,
        parse_absolute_redirect_query, parse_loopback_callback_target, parse_oauth_callback_query,
        parse_remote_authorization_input,
    };
    use crate::oauth_pins::{browser_test_pins, oidc_test_pins};
    use crate::oauth_workflow::OAuthWorkflowError;
    use zeroize::Zeroizing;

    #[test]
    fn browser_policy_accepts_success_and_error() {
        let pins = browser_test_pins();
        let policy = CallbackQueryPolicy::codex(pins.scopes);
        let state = "state-value-state-value-state-value-01";
        let success = parse_oauth_callback_query(
            &format!(
                "code=auth-code&state={state}&scope={}",
                urlencoding_scope(pins.scopes)
            ),
            policy,
        )
        .expect("success");
        assert_eq!(
            success,
            OAuthCallbackOutcome::AuthorizationCode {
                code: Zeroizing::new(String::from("auth-code")),
                state: Zeroizing::new(String::from(state)),
            }
        );
        let error = parse_oauth_callback_query(
            &format!("error=access_denied&error_description=cancelled&state={state}"),
            policy,
        )
        .expect("error");
        assert_eq!(
            error,
            OAuthCallbackOutcome::ProviderError {
                state: Zeroizing::new(String::from(state)),
            }
        );
        assert_eq!(
            parse_oauth_callback_query(
                &format!("code=auth-code&state={state}&scope=openid"),
                policy,
            ),
            Err(OAuthWorkflowError::Callback)
        );
    }

    #[test]
    fn oidc_policy_parses_absolute_redirect() {
        let pins = oidc_test_pins();
        let policy = CallbackQueryPolicy::xai(pins.scopes);
        let state = "state-value-state-value-state-value-01";
        let full = format!(
            "{}?code=browser-code&state={state}",
            pins.loopback_redirect_uri
        );
        let outcome = parse_absolute_redirect_query(&full, pins.loopback_redirect_uri, policy)
            .expect("absolute");
        assert_eq!(
            outcome,
            OAuthCallbackOutcome::AuthorizationCode {
                code: Zeroizing::new(String::from("browser-code")),
                state: Zeroizing::new(String::from(state)),
            }
        );
        assert_eq!(
            parse_absolute_redirect_query(
                &format!("{}?code=x&state={state}#frag", pins.loopback_redirect_uri),
                pins.loopback_redirect_uri,
                policy,
            ),
            Err(OAuthWorkflowError::Callback)
        );
        assert_eq!(
            parse_absolute_redirect_query(
                "http://evil.example/callback?code=x&state=y",
                pins.loopback_redirect_uri,
                policy,
            ),
            Err(OAuthWorkflowError::Callback)
        );
    }

    #[test]
    fn remote_input_accepts_exact_redirect_or_attempt_bound_bare_code() {
        let pins = browser_test_pins();
        let policy = CallbackQueryPolicy::codex(pins.scopes);
        let state = "attempt-state-attempt-state-attempt-state";
        let bare = "opaque/code+safe_value=1234";
        assert_eq!(
            parse_remote_authorization_input(bare, state, pins.loopback_redirect_uri, policy,)
                .expect("bare code"),
            OAuthCallbackOutcome::AuthorizationCode {
                code: Zeroizing::new(bare.to_owned()),
                state: Zeroizing::new(state.to_owned()),
            }
        );
        let full = format!(
            "{}?code=full-code&state={state}",
            pins.loopback_redirect_uri
        );
        assert!(
            parse_remote_authorization_input(&full, state, pins.loopback_redirect_uri, policy,)
                .is_ok()
        );
    }

    #[test]
    fn remote_input_rejection_is_payload_free() {
        let pins = browser_test_pins();
        let secret = "secret-code?with=query";
        let error = parse_remote_authorization_input(
            secret,
            "attempt-state-attempt-state-attempt-state",
            pins.loopback_redirect_uri,
            CallbackQueryPolicy::codex(pins.scopes),
        )
        .expect_err("invalid input");
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn loopback_callback_host_and_path_derive_from_the_redirect_used() {
        let policy = CallbackQueryPolicy::codex("openid");
        let redirect = "http://localhost:49152/auth/callback";
        assert_eq!(
            loopback_redirect_authority(redirect),
            Some("localhost:49152")
        );
        for rejected in ["https://localhost:1/cb", "http:///cb", "http://localhost:1"] {
            assert_eq!(loopback_redirect_authority(rejected), None, "{rejected}");
        }
        let target = "/auth/callback?code=c&state=s";
        assert!(matches!(
            parse_loopback_callback_target("localhost:49152", target, redirect, policy),
            Ok(OAuthCallbackOutcome::AuthorizationCode { .. })
        ));
        for (host, target) in [
            ("localhost:1455", target),
            ("127.0.0.1:49152", target),
            ("localhost:49153", target),
            ("localhost:49152", "/other?code=c&state=s"),
            ("localhost:49152", "/auth/callback"),
            ("localhost:49152", "/auth/callback?"),
        ] {
            assert_eq!(
                parse_loopback_callback_target(host, target, redirect, policy),
                Err(OAuthWorkflowError::Callback),
                "{host}{target}"
            );
        }
    }

    fn urlencoding_scope(scope: &str) -> String {
        url::form_urlencoded::byte_serialize(scope.as_bytes()).collect()
    }
}
