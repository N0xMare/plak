//! Pure helpers for an OAuth-protected resource's client.
//!
//! The pieces the MCP authorization specification profiles: the
//! `WWW-Authenticate` Bearer challenge (RFC 6750, RFC 9728 §5.1),
//! protected-resource metadata (RFC 9728), authorization-server metadata
//! (RFC 8414 and OIDC discovery), resource indicators (RFC 8707),
//! issuer identification (RFC 9207), dynamic client registration (RFC 7591),
//! the device grant (RFC 8628) and token revocation (RFC 7009). Every
//! function is credential-free string and JSON work; transport, storage and
//! registration policy belong to the caller.

use serde::Deserialize;
use url::Url;
use zeroize::Zeroizing;

use super::{OAuthWorkflowError, decode_json_bounded};
use crate::SecretString;

/// Longest challenge parameter value accepted.
const MAX_CHALLENGE_VALUE_BYTES: usize = 2048;
/// Largest metadata or token response accepted.
pub const MAX_RESOURCE_OAUTH_RESPONSE_BYTES: usize = 256 * 1024;
const PROTECTED_RESOURCE_WELL_KNOWN: &str = "/.well-known/oauth-protected-resource";
const AUTHORIZATION_SERVER_WELL_KNOWN: &str = "/.well-known/oauth-authorization-server";
const OPENID_WELL_KNOWN: &str = "/.well-known/openid-configuration";

/// The parameters of a `WWW-Authenticate` Bearer challenge a resource
/// client acts on.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BearerChallenge {
    /// `resource_metadata`: where the protected-resource metadata lives.
    pub resource_metadata: Option<String>,
    /// `scope`: the scope the request needed.
    pub scope: Option<String>,
    /// `error`, such as `invalid_token` or `insufficient_scope`.
    pub error: Option<String>,
}

/// Parses the Bearer challenge of one `WWW-Authenticate` value, which may
/// carry several challenges.
///
/// Returns `None` when no well-formed Bearer challenge is present. A Bearer
/// parameter given twice, or over 2 KiB, voids the challenge.
#[must_use]
pub fn parse_bearer_challenge(value: &str) -> Option<BearerChallenge> {
    let pieces = lex_challenges(value)?;
    let mut index = 0;
    while index < pieces.len() {
        while matches!(pieces.get(index), Some(Piece::Comma)) {
            index += 1;
        }
        let Some(Piece::Token(scheme)) = pieces.get(index) else {
            return None;
        };
        index += 1;
        let bearer = scheme.eq_ignore_ascii_case("bearer");
        let mut challenge = BearerChallenge::default();
        let mut seen = Vec::new();
        while let (Some(Piece::Token(name)), Some(Piece::Equals)) =
            (pieces.get(index), pieces.get(index + 1))
        {
            let value = match pieces.get(index + 2) {
                Some(Piece::Token(value)) => (*value).to_owned(),
                Some(Piece::Quoted(value)) => value.clone(),
                _ => return None,
            };
            index += 3;
            let name = name.to_ascii_lowercase();
            if bearer {
                if seen.contains(&name) || value.len() > MAX_CHALLENGE_VALUE_BYTES {
                    return None;
                }
                match name.as_str() {
                    "resource_metadata" => challenge.resource_metadata = Some(value),
                    "scope" if valid_scope_list(&value) => challenge.scope = Some(value),
                    "scope" => return None,
                    "error" => challenge.error = Some(value),
                    _ => {}
                }
                seen.push(name);
            }
            if !matches!(pieces.get(index), Some(Piece::Comma)) {
                break;
            }
            index += 1;
        }
        if bearer {
            return Some(challenge);
        }
        // Skip a token68 or anything else up to the next scheme.
        while let Some(piece) = pieces.get(index) {
            if let (Piece::Token(_), Some(Piece::Token(_) | Piece::Comma) | None) =
                (piece, pieces.get(index + 1))
            {
                break;
            }
            index += 1;
        }
    }
    None
}

enum Piece<'a> {
    Token(&'a str),
    Quoted(String),
    Equals,
    Comma,
}

fn lex_challenges(value: &str) -> Option<Vec<Piece<'_>>> {
    let bytes = value.as_bytes();
    let mut pieces = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' => index += 1,
            b',' => {
                pieces.push(Piece::Comma);
                index += 1;
            }
            b'=' => {
                pieces.push(Piece::Equals);
                index += 1;
            }
            b'"' => {
                index += 1;
                let mut quoted = String::new();
                loop {
                    match bytes.get(index)? {
                        b'"' => break,
                        b'\\' => {
                            index += 1;
                            quoted.push(char::from(*bytes.get(index)?));
                        }
                        byte if (0x20..=0x7e).contains(byte) || *byte == b'\t' => {
                            quoted.push(char::from(*byte));
                        }
                        _ => return None,
                    }
                    index += 1;
                }
                index += 1;
                pieces.push(Piece::Quoted(quoted));
            }
            byte if is_token_byte(byte) => {
                let start = index;
                while bytes.get(index).copied().is_some_and(is_token_byte) {
                    index += 1;
                }
                pieces.push(Piece::Token(&value[start..index]));
            }
            _ => return None,
        }
    }
    Some(pieces)
}

const fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
                | b'/'
                | b':'
        )
}

/// The canonical URI of a protected resource (RFC 8707, MCP authorization):
/// scheme and host lowercased, the default port and a bare `/` path dropped.
///
/// # Errors
///
/// Refuses a URL that is not absolute `https://` or `http://`, or that
/// carries credentials, a query or a fragment.
pub fn canonical_resource_uri(url: &str) -> Result<String, OAuthWorkflowError> {
    let parsed = Url::parse(url).map_err(|_| OAuthWorkflowError::InvalidUrl)?;
    if !matches!(parsed.scheme(), "https" | "http")
        || parsed.host_str().is_none_or(str::is_empty)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(OAuthWorkflowError::InvalidUrl);
    }
    let root = parsed.path() == "/";
    let mut canonical = String::from(parsed);
    if root {
        canonical.pop();
    }
    Ok(canonical)
}

/// The protected-resource metadata URLs to try for a resource without a
/// `resource_metadata` challenge, in the specification's order: the
/// path-aware well-known URL, then the origin's.
///
/// # Errors
///
/// Refuses a resource that is not a canonical resource URI.
pub fn protected_resource_metadata_urls(resource: &str) -> Result<Vec<String>, OAuthWorkflowError> {
    let url = Url::parse(&canonical_resource_uri(resource)?)
        .map_err(|_| OAuthWorkflowError::InvalidUrl)?;
    let origin = url.origin().ascii_serialization();
    let path = url.path();
    let mut urls = Vec::with_capacity(2);
    if !path.is_empty() && path != "/" {
        urls.push(format!("{origin}{PROTECTED_RESOURCE_WELL_KNOWN}{path}"));
    }
    urls.push(format!("{origin}{PROTECTED_RESOURCE_WELL_KNOWN}"));
    Ok(urls)
}

/// The protected-resource metadata fields a client uses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtectedResourceMetadata {
    /// The resource identifier exactly as published by the metadata: the
    /// `resource=` value of every authorization and token request.
    pub resource: String,
    /// The authorization server issuers it lists, in order.
    pub authorization_servers: Vec<String>,
    /// The scopes it advertises.
    pub scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct ProtectedResourceWire {
    resource: String,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// Parses protected-resource metadata covering the requested resource URI.
///
/// The metadata's resource must have the same origin and an equal or
/// path-segment-prefix path only at its own RFC 9728 well-known URL; elsewhere
/// equality is required (ignoring one trailing slash). It must list an
/// authorization server. Plain HTTP is limited to loopback. The published
/// value is retained byte-for-byte for resource indicators.
///
/// # Errors
///
/// [`OAuthWorkflowError::InvalidJson`] or
/// [`OAuthWorkflowError::ResponseTooLarge`] for a malformed body,
/// [`OAuthWorkflowError::ResourceMismatch`] for another resource and
/// [`OAuthWorkflowError::MissingMetadata`] without an authorization server.
pub fn parse_protected_resource_metadata(
    body: &[u8],
    resource_uri: &str,
    metadata_url: &str,
) -> Result<ProtectedResourceMetadata, OAuthWorkflowError> {
    let wire: ProtectedResourceWire = decode_json_bounded(body, MAX_RESOURCE_OAUTH_RESPONSE_BYTES)?;
    canonical_resource_uri(&wire.resource).map_err(|_| OAuthWorkflowError::ResourceMismatch)?;
    let published = Url::parse(&wire.resource).map_err(|_| OAuthWorkflowError::ResourceMismatch)?;
    // Only scheme/host case and default-port normalization are harmless here.
    // WHATWG path repair must not silently broaden a published audience.
    let raw_path = wire
        .resource
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|index| &rest[index..]))
        .unwrap_or("/");
    let authority = wire
        .resource
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""));
    let origin = published.origin().ascii_serialization();
    let normalized_authority = origin
        .split_once("://")
        .map_or("", |(_, authority)| authority);
    let default_port = if published.scheme() == "https" {
        443
    } else {
        80
    };
    let authority_matches = authority.is_some_and(|authority| {
        authority.eq_ignore_ascii_case(normalized_authority)
            || (published.port().is_none()
                && authority
                    .eq_ignore_ascii_case(&format!("{normalized_authority}:{default_port}")))
    });
    let lower_path = raw_path.to_ascii_lowercase();
    if wire
        .resource
        .chars()
        .any(|c| c.is_control() || !c.is_ascii() || c.is_whitespace() || c == '\\')
        || !authority_matches
        || raw_path != published.path()
        || lower_path.contains("%2f")
        || lower_path.contains("%5c")
    {
        return Err(OAuthWorkflowError::ResourceMismatch);
    }
    let endpoint = Url::parse(resource_uri).map_err(|_| OAuthWorkflowError::ResourceMismatch)?;
    let path = published
        .path()
        .strip_suffix('/')
        .unwrap_or(published.path());
    let endpoint_path = endpoint.path().strip_suffix('/').unwrap_or(endpoint.path());
    let own_metadata = format!(
        "{}{}{}",
        published.origin().ascii_serialization(),
        PROTECTED_RESOURCE_WELL_KNOWN,
        path
    );
    let location_bound = Url::parse(metadata_url).ok() == Url::parse(&own_metadata).ok();
    let loopback = matches!(published.host_str(), Some("127.0.0.1" | "[::1]"));
    if published.origin() != endpoint.origin()
        || (published.scheme() == "http" && !loopback)
        || !(endpoint_path == path
            || location_bound
                && endpoint_path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/')))
    {
        return Err(OAuthWorkflowError::ResourceMismatch);
    }
    if wire.authorization_servers.is_empty() {
        return Err(OAuthWorkflowError::MissingMetadata);
    }
    Ok(ProtectedResourceMetadata {
        resource: wire.resource,
        authorization_servers: wire.authorization_servers,
        scopes_supported: wire
            .scopes_supported
            .into_iter()
            .filter(|scope| valid_scope_token(scope))
            .collect(),
    })
}

/// The authorization-server metadata URLs to try for `issuer`, in the MCP
/// specification's order: RFC 8414 then OIDC discovery, with the
/// path inserted after the well-known segment, and for an issuer with a path
/// also OIDC's appended form.
///
/// # Errors
///
/// Refuses an issuer that is not an absolute URL without query or fragment.
pub fn authorization_server_metadata_urls(issuer: &str) -> Result<Vec<String>, OAuthWorkflowError> {
    let url = Url::parse(issuer).map_err(|_| OAuthWorkflowError::InvalidUrl)?;
    if !matches!(url.scheme(), "https" | "http")
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(OAuthWorkflowError::InvalidUrl);
    }
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    Ok(if path.is_empty() {
        vec![
            format!("{origin}{AUTHORIZATION_SERVER_WELL_KNOWN}"),
            format!("{origin}{OPENID_WELL_KNOWN}"),
        ]
    } else {
        vec![
            format!("{origin}{AUTHORIZATION_SERVER_WELL_KNOWN}{path}"),
            format!("{origin}{OPENID_WELL_KNOWN}{path}"),
            format!("{origin}{path}{OPENID_WELL_KNOWN}"),
        ]
    })
}

/// The authorization-server metadata fields a resource client uses.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthorizationServerMetadata {
    /// The issuer, identical to the one the metadata URL was built from.
    pub issuer: String,
    /// The authorization endpoint.
    pub authorization_endpoint: String,
    /// The token endpoint.
    pub token_endpoint: String,
    /// The dynamic client registration endpoint, when offered.
    pub registration_endpoint: Option<String>,
    /// The token revocation endpoint, when offered.
    pub revocation_endpoint: Option<String>,
    /// The device authorization endpoint, when offered.
    pub device_authorization_endpoint: Option<String>,
    /// The scopes it advertises.
    pub scopes_supported: Vec<String>,
    /// The token endpoint client authentication methods it lists.
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// Whether every authorization response carries `iss` (RFC 9207).
    pub authorization_response_iss_parameter_supported: bool,
    /// Whether it accepts Client ID Metadata Document client ids.
    pub client_id_metadata_document_supported: bool,
}

#[derive(Deserialize)]
struct AuthorizationServerWire {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    device_authorization_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
    #[serde(default)]
    client_id_metadata_document_supported: bool,
}

/// Parses authorization-server metadata fetched for `issuer`.
///
/// # Errors
///
/// [`OAuthWorkflowError::IssuerMismatch`] when the document's `issuer` is
/// not identical to `issuer` (the document is not to be used),
/// [`OAuthWorkflowError::PkceUnsupported`] when
/// `code_challenge_methods_supported` is absent or lacks `S256` (the
/// client must refuse to authorize), and the JSON errors for a malformed
/// body.
pub fn parse_authorization_server_metadata(
    body: &[u8],
    issuer: &str,
) -> Result<AuthorizationServerMetadata, OAuthWorkflowError> {
    let wire: AuthorizationServerWire =
        decode_json_bounded(body, MAX_RESOURCE_OAUTH_RESPONSE_BYTES)?;
    if wire.issuer != issuer {
        return Err(OAuthWorkflowError::IssuerMismatch);
    }
    if !wire
        .code_challenge_methods_supported
        .is_some_and(|methods| methods.iter().any(|method| method == "S256"))
    {
        return Err(OAuthWorkflowError::PkceUnsupported);
    }
    Ok(AuthorizationServerMetadata {
        issuer: wire.issuer,
        authorization_endpoint: wire.authorization_endpoint,
        token_endpoint: wire.token_endpoint,
        registration_endpoint: wire.registration_endpoint,
        revocation_endpoint: wire.revocation_endpoint,
        device_authorization_endpoint: wire.device_authorization_endpoint,
        scopes_supported: wire
            .scopes_supported
            .into_iter()
            .filter(|scope| valid_scope_token(scope))
            .collect(),
        token_endpoint_auth_methods_supported: wire.token_endpoint_auth_methods_supported,
        authorization_response_iss_parameter_supported: wire
            .authorization_response_iss_parameter_supported,
        client_id_metadata_document_supported: wire.client_id_metadata_document_supported,
    })
}

/// Inputs of a PKCE authorization request to a protected resource's
/// authorization server.
#[derive(Clone, Copy, Debug)]
pub struct ResourceAuthorizeRequest<'a> {
    /// The authorization endpoint; any query it carries is kept.
    pub authorization_endpoint: &'a str,
    /// The client id.
    pub client_id: &'a str,
    /// The exact redirect URI of this attempt.
    pub redirect_uri: &'a str,
    /// The requested scope, when any.
    pub scope: Option<&'a str>,
    /// The S256 code challenge.
    pub code_challenge: &'a str,
    /// The `state` correlation value.
    pub state: &'a str,
    /// The `resource` indicator.
    pub resource: &'a str,
}

/// Builds the authorization URL: `response_type`, `client_id`,
/// `redirect_uri`, optional `scope`, `code_challenge`,
/// `code_challenge_method=S256`, `state` and `resource`, appended to the
/// endpoint's own query.
///
/// # Errors
///
/// [`OAuthWorkflowError::InvalidUrl`] for an endpoint that is not an absolute
/// HTTP(S) URL or an empty required input.
pub fn build_resource_authorization_url(
    request: ResourceAuthorizeRequest<'_>,
) -> Result<Zeroizing<String>, OAuthWorkflowError> {
    if [
        request.client_id,
        request.redirect_uri,
        request.code_challenge,
        request.state,
        request.resource,
    ]
    .iter()
    .any(|value| value.is_empty())
        || request.scope.is_some_and(str::is_empty)
    {
        return Err(OAuthWorkflowError::InvalidUrl);
    }
    let mut url =
        Url::parse(request.authorization_endpoint).map_err(|_| OAuthWorkflowError::InvalidUrl)?;
    if !matches!(url.scheme(), "https" | "http") || url.fragment().is_some() {
        return Err(OAuthWorkflowError::InvalidUrl);
    }
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("response_type", "code")
            .append_pair("client_id", request.client_id)
            .append_pair("redirect_uri", request.redirect_uri);
        if let Some(scope) = request.scope {
            pairs.append_pair("scope", scope);
        }
        pairs
            .append_pair("code_challenge", request.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", request.state)
            .append_pair("resource", request.resource);
    }
    Ok(Zeroizing::new(url.into()))
}

/// Removes the RFC 9207 `iss` parameter from a callback URL or query.
///
/// Returns the input without `iss`, every other field byte for byte, and the
/// decoded issuer when present.
///
/// # Errors
///
/// [`OAuthWorkflowError::Callback`] when `iss` is given twice or empty.
pub fn take_issuer_parameter(
    callback: &str,
) -> Result<(Zeroizing<String>, Option<Zeroizing<String>>), OAuthWorkflowError> {
    let (prefix, query) = match callback.split_once('?') {
        Some((prefix, query)) => (Some(prefix), query),
        None => (None, callback),
    };
    let mut issuer = None;
    let mut kept = Vec::new();
    for field in query.split('&') {
        let mut decoded = url::form_urlencoded::parse(field.as_bytes());
        match decoded.next() {
            Some((key, value)) if key == "iss" => {
                if issuer.is_some() || value.is_empty() {
                    return Err(OAuthWorkflowError::Callback);
                }
                issuer = Some(Zeroizing::new(value.into_owned()));
            }
            _ => kept.push(field),
        }
    }
    let rest = kept.join("&");
    let rebuilt = match prefix {
        Some(prefix) => format!("{prefix}?{rest}"),
        None => rest,
    };
    Ok((Zeroizing::new(rebuilt), issuer))
}

/// Checks a callback's `iss` against the issuer recorded before the browser
/// opened (RFC 9207): equal by simple string comparison when present;
/// absent only when the server does not promise it.
///
/// # Errors
///
/// [`OAuthWorkflowError::IssuerMismatch`] for a different issuer or a
/// missing one the metadata promised.
pub fn check_callback_issuer(
    returned: Option<&str>,
    expected: &str,
    required: bool,
) -> Result<(), OAuthWorkflowError> {
    match returned {
        Some(returned) if returned == expected => Ok(()),
        None if !required => Ok(()),
        _ => Err(OAuthWorkflowError::IssuerMismatch),
    }
}

/// The authorization-code token request fields with a resource indicator.
#[must_use]
pub fn resource_token_form<'a>(
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    resource: &'a str,
) -> [(&'static str, &'a str); 6] {
    [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
        ("resource", resource),
    ]
}

/// The refresh-token grant fields with a resource indicator.
#[must_use]
pub fn resource_refresh_form<'a>(
    refresh_token: &'a str,
    client_id: &'a str,
    resource: &'a str,
) -> [(&'static str, &'a str); 4] {
    [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("resource", resource),
    ]
}

/// The device-code token request fields (RFC 8628 §3.4).
#[must_use]
pub fn device_token_form<'a>(
    device_code: &'a str,
    client_id: &'a str,
    resource: &'a str,
) -> [(&'static str, &'a str); 4] {
    [
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("device_code", device_code),
        ("client_id", client_id),
        ("resource", resource),
    ]
}

/// The RFC 7009 revocation request fields.
#[must_use]
pub fn revocation_form<'a>(
    token: &'a str,
    token_type_hint: &'static str,
    client_id: &'a str,
) -> [(&'static str, &'a str); 3] {
    [
        ("token", token),
        ("token_type_hint", token_type_hint),
        ("client_id", client_id),
    ]
}

/// One successful token response. Token values are secret.
#[derive(Debug)]
pub struct ResourceTokenResponse {
    /// The access token.
    pub access_token: SecretString,
    /// Seconds until the access token expires, when given.
    pub expires_in: Option<u64>,
    /// The refresh token, when given.
    pub refresh_token: Option<SecretString>,
    /// The granted scope, when given.
    pub scope: Option<String>,
}

#[derive(Deserialize)]
struct TokenWire {
    access_token: String,
    token_type: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    scope: Option<String>,
}

impl Drop for TokenWire {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.access_token.zeroize();
        if let Some(refresh) = &mut self.refresh_token {
            refresh.zeroize();
        }
    }
}

/// Parses a successful token response; `token_type` must be `Bearer` and
/// the tokens visible ASCII.
///
/// # Errors
///
/// [`OAuthWorkflowError::InvalidJson`] for any other shape.
pub fn parse_resource_token_response(
    body: &[u8],
) -> Result<ResourceTokenResponse, OAuthWorkflowError> {
    let mut wire: TokenWire = decode_json_bounded(body, MAX_RESOURCE_OAUTH_RESPONSE_BYTES)?;
    let visible = |token: &str| {
        !token.is_empty()
            && token.len() <= 16 * 1024
            && token.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    };
    if !wire.token_type.eq_ignore_ascii_case("bearer")
        || !visible(&wire.access_token)
        || wire
            .refresh_token
            .as_deref()
            .is_some_and(|token| !visible(token))
    {
        return Err(OAuthWorkflowError::InvalidJson);
    }
    Ok(ResourceTokenResponse {
        access_token: SecretString::new(std::mem::take(&mut wire.access_token)),
        expires_in: wire.expires_in,
        refresh_token: wire.refresh_token.take().map(SecretString::new),
        scope: wire.scope.take().filter(|scope| valid_scope_list(scope)),
    })
}

/// The `error` code of an OAuth error response (RFC 6749 §5.2), such as
/// `invalid_grant`, `authorization_pending` or `slow_down`.
#[must_use]
pub fn oauth_error_code(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct ErrorWire {
        error: String,
    }
    decode_json_bounded::<ErrorWire>(body, MAX_RESOURCE_OAUTH_RESPONSE_BYTES)
        .ok()
        .map(|wire| wire.error)
        .filter(|code| {
            !code.is_empty()
                && code.len() <= 128
                && code.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        })
}

/// The fields of one device authorization request (RFC 8628 §3.1).
#[must_use]
pub fn device_authorization_form<'a>(
    client_id: &'a str,
    scope: Option<&'a str>,
    resource: &'a str,
) -> Vec<(&'static str, &'a str)> {
    let mut fields = vec![("client_id", client_id)];
    if let Some(scope) = scope {
        fields.push(("scope", scope));
    }
    fields.push(("resource", resource));
    fields
}

/// One device authorization response.
#[derive(Debug)]
pub struct DeviceAuthorization {
    /// The device code, secret.
    pub device_code: SecretString,
    /// The code the user enters.
    pub user_code: String,
    /// Where the user enters it.
    pub verification_uri: String,
    /// Seconds the codes stay valid.
    pub expires_in: u64,
    /// Seconds between polls, when given.
    pub interval: Option<u64>,
}

/// Parses a device authorization response.
///
/// # Errors
///
/// [`OAuthWorkflowError::InvalidJson`] for any other shape.
pub fn parse_device_authorization(body: &[u8]) -> Result<DeviceAuthorization, OAuthWorkflowError> {
    #[derive(Deserialize)]
    struct DeviceWire {
        device_code: String,
        user_code: String,
        verification_uri: String,
        expires_in: u64,
        interval: Option<u64>,
    }
    let wire: DeviceWire = decode_json_bounded(body, MAX_RESOURCE_OAUTH_RESPONSE_BYTES)?;
    let printable = |value: &str| {
        !value.is_empty()
            && value.len() <= 2048
            && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    };
    if !printable(&wire.device_code)
        || !printable(&wire.user_code)
        || !printable(&wire.verification_uri)
        || wire.expires_in == 0
    {
        return Err(OAuthWorkflowError::InvalidJson);
    }
    Ok(DeviceAuthorization {
        device_code: SecretString::new(wire.device_code),
        user_code: wire.user_code,
        verification_uri: wire.verification_uri,
        expires_in: wire.expires_in,
        interval: wire.interval,
    })
}

/// The RFC 7591 registration request of a native public client for exactly
/// `redirect_uri`, including the device grant only when the caller admits it
/// for this attempt.
#[must_use]
pub fn dynamic_registration_request(
    client_name: &str,
    redirect_uri: &str,
    device_grant: bool,
) -> Vec<u8> {
    let mut grants = vec!["authorization_code", "refresh_token"];
    if device_grant {
        grants.push("urn:ietf:params:oauth:grant-type:device_code");
    }
    serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": grants,
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    })
    .to_string()
    .into_bytes()
}

/// One dynamic registration response.
#[derive(Debug)]
pub struct DynamicRegistration {
    /// The issued client id.
    pub client_id: String,
    /// The issued client secret, when the server made the client
    /// confidential.
    pub client_secret: Option<SecretString>,
}

/// Parses a dynamic registration response.
///
/// # Errors
///
/// [`OAuthWorkflowError::InvalidJson`] for any other shape.
pub fn parse_dynamic_registration(body: &[u8]) -> Result<DynamicRegistration, OAuthWorkflowError> {
    #[derive(Deserialize)]
    struct RegistrationWire {
        client_id: String,
        client_secret: Option<String>,
    }
    let wire: RegistrationWire = decode_json_bounded(body, MAX_RESOURCE_OAUTH_RESPONSE_BYTES)?;
    if wire.client_id.is_empty()
        || wire.client_id.len() > 1024
        || !wire
            .client_id
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(OAuthWorkflowError::InvalidJson);
    }
    Ok(DynamicRegistration {
        client_id: wire.client_id,
        client_secret: wire
            .client_secret
            .filter(|secret| !secret.is_empty())
            .map(SecretString::new),
    })
}

/// Whether a scope list contains only RFC 6749 scope tokens in the
/// conservative display alphabet (letters, digits and `._:/+-`).
#[must_use]
pub fn valid_scope_list(scope: &str) -> bool {
    !scope.is_empty() && scope.split(' ').all(valid_scope_token)
}

fn valid_scope_token(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'+' | b'-')
        })
}

/// The union of space-separated scope lists, each scope once, in first-seen
/// order; `None` when empty.
#[must_use]
pub fn union_scopes<'a>(scopes: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    let mut united: Vec<&str> = Vec::new();
    for scope in scopes.into_iter().flatten().flat_map(str::split_whitespace) {
        if !united.contains(&scope) {
            united.push(scope);
        }
    }
    (!united.is_empty()).then(|| united.join(" "))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
