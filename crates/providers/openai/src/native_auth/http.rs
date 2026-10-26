//! Bounded auth and catalog HTTP exchanges, headers and JSON bodies.

use super::{
    Arc, CODEX_ORIGINATOR, CancellationToken, ClaimedHttpResponse, Client, Deserialize,
    EndpointOrigin, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Instant,
    MAX_AUTH_BODY_BYTES, MAX_CATALOG_BODY_BYTES, MAX_TOKEN_BYTES, Method, NativeCodexAuthError,
    Origin, PullBodyError, Request, SensitiveBytes, StatusCode, SubscriptionRequestGate,
    USER_AGENT, Url, Zeroizing, decode_json_bounded, map_oauth_workflow_error, network_failure,
    pull_claimed_body, sensitive_header_value, urlencoded_form_bytes, validate_oauth_value,
};
use plakous_auth::oauth_orchestrator::{AuthBodyError, is_json_media_type, pull_auth_body};

pub(super) struct HttpReply {
    pub(super) status: StatusCode,
    pub(super) cache_control: Option<String>,
    pub(super) content_type: Option<String>,
    pub(super) body: Zeroizing<Vec<u8>>,
}

struct HttpAttemptError(Option<plakous_model::ModelNetworkFailure>);

pub(super) async fn execute_bounded(
    client: &Client,
    origin: &EndpointOrigin,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    deadline: Instant,
) -> Result<HttpReply, NativeCodexAuthError> {
    execute_classified(
        client,
        origin,
        method,
        path,
        headers,
        body,
        deadline,
        MAX_AUTH_BODY_BYTES,
    )
    .await
    .map_err(|error| NativeCodexAuthError::Transport(error.0))
}

pub(super) async fn execute_catalog_bounded(
    client: &Client,
    origin: &EndpointOrigin,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    budget: plakous_exchange::lookup::LookupBudget,
    gate: Option<Arc<dyn SubscriptionRequestGate>>,
) -> Result<HttpReply, NativeCodexAuthError> {
    let request = Request::new(
        Origin::parse(origin.as_str()).map_err(|_| NativeCodexAuthError::Validation)?,
        method,
        path,
        headers,
        SensitiveBytes::new(body).into_bytes(),
        client.limits(),
    )
    .map_err(|_| NativeCodexAuthError::Validation)?;
    let mut dispatch = plakous_auth::subscription_limit::CatalogRequestDispatch::new(gate);
    let reply = plakous_exchange::lookup::get_with_dispatch(
        client,
        request,
        budget,
        MAX_CATALOG_BODY_BYTES,
        &mut |failure| {
            dispatch
                .observe(failure.map(plakous_http::Failure::transmission))
                .map_err(|_| plakous_exchange::lookup::LookupError::DispatchDenied)
        },
    )
    .await
    .map_err(|error| match error {
        plakous_exchange::lookup::LookupError::Transport(failure) => {
            NativeCodexAuthError::Transport(network_failure(failure))
        }
        plakous_exchange::lookup::LookupError::Exhausted { .. } => {
            NativeCodexAuthError::Lookup(error)
        }
        plakous_exchange::lookup::LookupError::DispatchDenied => NativeCodexAuthError::Catalog,
        _ => NativeCodexAuthError::Transport(None),
    })?;
    Ok(HttpReply {
        status: reply.status,
        cache_control: reply
            .headers
            .get(&HeaderName::from_static("cache-control"))
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 1024)
            .map(str::to_owned),
        content_type: reply
            .headers
            .get(&HeaderName::from_static("content-type"))
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 128)
            .map(str::to_owned),
        body: reply.body,
    })
}

async fn execute_classified(
    client: &Client,
    origin: &EndpointOrigin,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    deadline: Instant,
    max_body_bytes: usize,
) -> Result<HttpReply, HttpAttemptError> {
    // The exact Vec allocation remains the backing owner across transport
    // Bytes clones and is wiped when the final shared owner drops.
    let body = SensitiveBytes::new(body).into_bytes();
    let origin = Origin::parse(origin.as_str()).map_err(|_| HttpAttemptError(None))?;
    let request = Request::new(origin, method, path, headers, body, client.limits())
        .map_err(|_| HttpAttemptError(None))?;
    let cancellation = CancellationToken::new();
    let response = client
        .execute(
            request,
            ExchangeControl::with_cancellation(deadline, cancellation),
        )
        .await
        .map_err(|failure| HttpAttemptError(network_failure(failure)))?;
    let status = response.status();
    let cache_control = response
        .headers()
        .get(&HeaderName::from_static("cache-control"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 1024)
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get(&HeaderName::from_static("content-type"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 128)
        .map(str::to_owned);
    let bytes = pull_auth_body(response.into_body(), max_body_bytes)
        .await
        .map_err(|error| {
            HttpAttemptError(match error {
                AuthBodyError::Failure(failure) => network_failure(failure),
                AuthBodyError::TooLarge => None,
            })
        })?;
    Ok(HttpReply {
        status,
        cache_control,
        content_type,
        body: bytes,
    })
}

pub(super) fn claimed_request(
    client: &Client,
    origin: &EndpointOrigin,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<Request, NativeCodexAuthError> {
    let origin = Origin::parse(origin.as_str()).map_err(|_| NativeCodexAuthError::Validation)?;
    Request::new(
        origin,
        method,
        path,
        headers,
        SensitiveBytes::new(body).into_bytes(),
        client.limits(),
    )
    .map_err(|_| NativeCodexAuthError::Validation)
}

pub(super) async fn pull_claimed_body_cancellable(
    response: &mut ClaimedHttpResponse,
    cancellation: Option<&plakous_model::CancellationToken>,
) -> Result<Zeroizing<Vec<u8>>, NativeCodexAuthError> {
    let Some(cancellation) = cancellation else {
        return pull_claimed_body(response, MAX_AUTH_BODY_BYTES)
            .await
            .map_err(|error| match error {
                PullBodyError::Transport => NativeCodexAuthError::Transport(None),
                PullBodyError::TooLarge => NativeCodexAuthError::Validation,
                _ => NativeCodexAuthError::Transport(None),
            });
    };
    let mut bytes = Zeroizing::new(Vec::new());
    loop {
        let chunk = {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(NativeCodexAuthError::Transport(None));
                }
                chunk = response.next_chunk() => chunk,
            }
        };
        match chunk {
            None => {
                if !response.body_completed_cleanly() {
                    return Err(NativeCodexAuthError::Transport(None));
                }
                return Ok(bytes);
            }
            Some(Err(_)) => return Err(NativeCodexAuthError::Transport(None)),
            Some(Ok(chunk)) => {
                if bytes.len().saturating_add(chunk.len()) > MAX_AUTH_BODY_BYTES {
                    return Err(NativeCodexAuthError::Validation);
                }
                bytes.extend_from_slice(&chunk);
            }
        }
    }
}

pub(super) fn require_status(
    reply: &HttpReply,
    expected: StatusCode,
) -> Result<(), NativeCodexAuthError> {
    if reply.status == expected {
        Ok(())
    } else {
        Err(NativeCodexAuthError::Provider)
    }
}

pub(super) fn require_json_media_type(reply: &HttpReply) -> Result<(), NativeCodexAuthError> {
    let is_json = reply
        .content_type
        .as_deref()
        .is_some_and(is_json_media_type);
    if is_json {
        Ok(())
    } else {
        Err(NativeCodexAuthError::Validation)
    }
}

pub(super) fn parse_strict_json<T>(bytes: &[u8]) -> Result<T, NativeCodexAuthError>
where
    T: for<'de> Deserialize<'de>,
{
    parse_json_bounded(bytes, MAX_AUTH_BODY_BYTES)
}

pub(super) fn parse_catalog_json<T>(bytes: &[u8]) -> Result<T, NativeCodexAuthError>
where
    T: for<'de> Deserialize<'de>,
{
    parse_json_bounded(bytes, MAX_CATALOG_BODY_BYTES)
}

fn parse_json_bounded<T>(bytes: &[u8], max_body_bytes: usize) -> Result<T, NativeCodexAuthError>
where
    T: for<'de> Deserialize<'de>,
{
    decode_json_bounded(bytes, max_body_bytes).map_err(map_oauth_workflow_error)
}

pub(super) fn endpoint_url(
    origin: &EndpointOrigin,
    path: &str,
) -> Result<Url, NativeCodexAuthError> {
    let mut url = Url::parse(origin.as_str()).map_err(|_| NativeCodexAuthError::Validation)?;
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

pub(super) fn base_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("accept"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("originator"),
        HeaderValue::from_static(CODEX_ORIGINATOR),
    );
    headers.insert(
        HeaderName::from_static("user-agent"),
        HeaderValue::from_static(USER_AGENT),
    );
    headers
}

pub(super) fn json_headers(bearer: Option<&str>) -> Result<HeaderMap, NativeCodexAuthError> {
    let mut headers = if let Some(token) = bearer {
        bearer_headers(token, false)?
    } else {
        base_headers()
    };
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    Ok(headers)
}

pub(super) fn form_headers() -> HeaderMap {
    // Keep the authorization-code exchange wire-compatible with the
    // provider-owned Codex client. `originator` belongs to the authorization
    // URL, not to the token endpoint request.
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    headers
}

pub(super) fn bearer_headers(token: &str, json: bool) -> Result<HeaderMap, NativeCodexAuthError> {
    validate_oauth_value(token, MAX_TOKEN_BYTES)?;
    let mut headers = base_headers();
    let mut bearer = Vec::with_capacity(7 + token.len());
    bearer.extend_from_slice(b"Bearer ");
    bearer.extend_from_slice(token.as_bytes());
    let value = sensitive_header_value(bearer).map_err(|_| NativeCodexAuthError::Validation)?;
    headers.insert(HeaderName::from_static("authorization"), value);
    if json {
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
    }
    Ok(headers)
}

pub(super) fn form_body(values: &[(&str, &str)]) -> Vec<u8> {
    let mut encoded = urlencoded_form_bytes(values);
    std::mem::take(&mut *encoded)
}
