//! [`HttpTransport`] over the bounded Plakous HTTP client.

use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use plakous_http::{Client, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Method};

use super::{
    HttpRequest, HttpResponse, HttpResponseFuture, HttpTransport, HttpTransportError,
    HttpTransportErrorKind,
};

/// [`HttpTransport`] over one bounded [`Client`], for absolute `https` URLs.
/// Each exchange is bounded by `deadline` up to its response head, and then
/// by the client's per-chunk idle timeout.
#[derive(Clone, Debug)]
pub struct NativeHttpTransport {
    client: Client,
    deadline: Duration,
}

impl NativeHttpTransport {
    /// A transport over `client`, bounding each exchange's head by `deadline`.
    #[must_use]
    pub const fn new(client: Client, deadline: Duration) -> Self {
        Self { client, deadline }
    }
}

fn invalid(message: &str) -> HttpTransportError {
    HttpTransportError::new(HttpTransportErrorKind::InvalidRequest, message)
}

fn wire_request(
    client: &Client,
    request: &HttpRequest,
) -> Result<plakous_http::Request, HttpTransportError> {
    let url = request.url();
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| invalid("request URL is not an absolute https URL"))?;
    let split = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    let origin = plakous_http::Origin::parse(&format!("https://{authority}/"))
        .map_err(|_| invalid("request URL has an invalid origin"))?;
    let path = if path.is_empty() { "/" } else { path };
    let method = Method::from_bytes(request.method().as_bytes())
        .map_err(|_| invalid("request method is invalid"))?;
    let mut headers = HeaderMap::new();
    for (name, value) in request.headers() {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid("request header name is invalid"))?;
        let value = HeaderValue::from_bytes(value)
            .map_err(|_| invalid("request header value is invalid"))?;
        headers.append(name, value);
    }
    plakous_http::Request::new(
        origin,
        method,
        path,
        headers,
        plakous_http::Bytes::copy_from_slice(request.body_bytes()),
        client.limits(),
    )
    .map_err(|_| invalid("request was refused by the transport limits"))
}

impl HttpTransport for NativeHttpTransport {
    fn send(&self, request: HttpRequest) -> HttpResponseFuture {
        let client = self.client.clone();
        let deadline = self.deadline;
        Box::pin(async move {
            let wire = wire_request(&client, &request)?;
            let control = ExchangeControl::streaming(
                Instant::now() + deadline,
                plakous_http::CancellationToken::new(),
            );
            let response = client.execute(wire, control).await.map_err(|failure| {
                HttpTransportError::new(HttpTransportErrorKind::Exchange, failure.to_string())
            })?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
                .collect();
            let body = futures_util::stream::unfold(
                Some(response.into_body()),
                |body| async move {
                    let mut body = body?;
                    let chunk = body.next_chunk().await?;
                    Some(match chunk {
                        Ok(bytes) => (Ok(bytes.to_vec()), Some(body)),
                        Err(failure) => (
                            Err(HttpTransportError::new(
                                HttpTransportErrorKind::Body,
                                failure.to_string(),
                            )),
                            None,
                        ),
                    })
                },
            )
            .boxed();
            Ok(HttpResponse::new(status, headers, body))
        })
    }
}
