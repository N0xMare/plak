use std::any::TypeId;

use plakous_http::{Phase, TransmissionFact};
use rmcp::transport::DynamicTransportError;

use super::*;
use crate::spec::McpServerSpec;

type TestResult = Result<(), Box<dyn Error>>;

fn refused(url: &str) -> bool {
    McpHttpEndpoint::new(url).is_err_and(|error| error.kind() == McpErrorKind::Definition)
}

#[test]
fn urls_are_https_or_plain_http_to_a_loopback_literal() -> TestResult {
    let remote = McpHttpEndpoint::new("https://mcp.example.com/v1/mcp?tenant=a")?;
    assert_eq!(remote.path_and_query, "/v1/mcp?tenant=a");
    assert!(!remote.origin.is_loopback_http());
    let local = McpHttpEndpoint::new("http://127.0.0.1:8931/mcp")?;
    assert!(local.origin.is_loopback_http());
    assert_eq!(local.origin.binding_authority(), "127.0.0.1:8931");
    assert!(
        McpHttpEndpoint::new("http://[::1]:8931")?
            .origin
            .is_loopback_http()
    );
    assert_eq!(
        McpHttpEndpoint::new("https://mcp.example.com")?.path_and_query,
        "/"
    );
    for url in [
        "http://localhost:8931/mcp",
        "http://10.0.0.1/mcp",
        "http://mcp.example.com/mcp",
        "ftp://mcp.example.com/",
        "https://user:pass@mcp.example.com/mcp",
        "https://mcp.example.com/mcp#frag",
        "/mcp",
        "http://127.0.0.1:0/mcp",
    ] {
        assert!(refused(url), "{url}");
    }
    Ok(())
}

fn resolver(values: Vec<(McpHttpSecret, &'static str)>) -> McpHttpSecretResolver {
    Arc::new(move || {
        Ok(values
            .iter()
            .map(|(slot, value)| (slot.clone(), SensitiveBytes::new(value.as_bytes().to_vec())))
            .collect())
    })
}

#[test]
fn header_declarations_keep_authorization_and_transport_headers_out() -> TestResult {
    let endpoint = || McpHttpEndpoint::new("https://mcp.example.com/mcp");
    let api_key = HeaderName::from_static("x-api-key");
    let literal =
        |name: &'static str| (HeaderName::from_static(name), HeaderValue::from_static("v"));
    assert!(
        endpoint()?
            .with_headers([literal("authorization")])
            .is_err()
    );
    assert!(
        endpoint()?
            .with_headers([literal("mcp-session-id")])
            .is_err()
    );
    assert!(endpoint()?.with_headers([literal("accept")]).is_err());
    assert!(
        endpoint()?
            .with_headers([literal("x-tenant")])?
            .with_secret_headers(
                [McpHttpSecret::Header(HeaderName::from_static("x-tenant"))],
                resolver(Vec::new()),
            )
            .is_err(),
        "a header is literal or secret, not both"
    );
    assert!(
        endpoint()?
            .with_secret_headers(
                [McpHttpSecret::Bearer, McpHttpSecret::Header(AUTHORIZATION)],
                resolver(Vec::new()),
            )
            .is_err()
    );
    endpoint()?
        .with_headers([literal("x-tenant")])?
        .with_secret_headers(
            [McpHttpSecret::Bearer, McpHttpSecret::Header(api_key)],
            resolver(Vec::new()),
        )?;
    endpoint()?
        .with_secret_headers([McpHttpSecret::Header(AUTHORIZATION)], resolver(Vec::new()))?;
    Ok(())
}

#[test]
fn the_digest_covers_the_endpoint_but_not_secret_values_and_debug_hides_values() -> TestResult {
    let name = McpServerName::new("remote")?;
    let api_key = HeaderName::from_static("x-api-key");
    let spec =
        |url: &str, tenant: &'static str, token: &'static str| -> Result<McpServerSpec, McpError> {
            Ok(McpServerSpec::http(
                name.clone(),
                McpHttpEndpoint::new(url)?
                    .with_headers([(
                        HeaderName::from_static("x-tenant"),
                        HeaderValue::from_static(tenant),
                    )])?
                    .with_secret_headers(
                        [
                            McpHttpSecret::Bearer,
                            McpHttpSecret::Header(api_key.clone()),
                        ],
                        resolver(vec![
                            (McpHttpSecret::Bearer, token),
                            (McpHttpSecret::Header(api_key.clone()), token),
                        ]),
                    )?,
            ))
        };
    let base = spec("https://mcp.example.com/mcp", "acme", "tok-one")?;
    assert_eq!(
        base.definition_digest()?,
        spec("https://mcp.example.com/mcp", "acme", "tok-two")?.definition_digest()?
    );
    for edited in [
        spec("https://mcp.example.com/other", "acme", "tok-one")?,
        spec("https://mcp.example.com/mcp", "other", "tok-one")?,
        McpServerSpec::http(
            name.clone(),
            McpHttpEndpoint::new("https://mcp.example.com/mcp")?,
        ),
        McpServerSpec::stdio(name.clone(), "https://mcp.example.com/mcp", []),
    ] {
        assert_ne!(
            base.definition_digest()?,
            edited.definition_digest()?,
            "{edited:?}"
        );
    }
    let debug = format!("{base:?}");
    assert!(debug.contains("x-api-key"), "{debug}");
    assert!(
        !debug.contains("acme") && !debug.contains("tok-one"),
        "{debug}"
    );
    Ok(())
}

#[test]
fn secrets_resolve_into_sensitive_headers_exactly_once_per_slot() -> TestResult {
    let server = McpServerName::new("remote")?;
    let api_key = HeaderName::from_static("x-api-key");
    let slots = [
        McpHttpSecret::Bearer,
        McpHttpSecret::Header(api_key.clone()),
    ];
    let base = Client::new(Limits::default())?;
    let endpoint = McpHttpEndpoint::new("https://mcp.example.com/mcp")?;
    let client = endpoint
        .clone()
        .with_secret_headers(
            slots.clone(),
            resolver(vec![
                (McpHttpSecret::Bearer, "tok"),
                (McpHttpSecret::Header(api_key.clone()), "key"),
            ]),
        )?
        .client(&server, &base, Duration::from_secs(5))?;
    let headers = &client.inner.headers;
    assert_eq!(
        headers.get(AUTHORIZATION).map(HeaderValue::as_bytes),
        Some(&b"Bearer tok"[..])
    );
    assert!(headers.values().all(HeaderValue::is_sensitive));
    assert!(!format!("{headers:?}").contains("tok"));
    assert!(client.inner.http.address_policy().is_none());
    for wrong in [
        vec![(McpHttpSecret::Bearer, "tok")],
        vec![
            (McpHttpSecret::Bearer, "tok"),
            (McpHttpSecret::Bearer, "tok"),
        ],
        vec![
            (McpHttpSecret::Bearer, "tok\n"),
            (McpHttpSecret::Header(api_key.clone()), "key"),
        ],
    ] {
        let refused = endpoint
            .clone()
            .with_secret_headers(slots.clone(), resolver(wrong))?
            .client(&server, &base, Duration::from_secs(5));
        let error = refused.err().ok_or("resolution accepted")?;
        assert_eq!(error.kind(), McpErrorKind::Definition);
        assert!(!error.to_string().contains("tok"), "{error}");
    }
    Ok(())
}

fn chunks(
    parts: Vec<Result<&'static [u8], Failure>>,
) -> impl Stream<Item = Result<Bytes, Failure>> + Send + 'static {
    stream::iter(parts.into_iter().map(|part| part.map(Bytes::from_static)))
}

#[tokio::test]
async fn sse_events_are_framed_without_ids_and_bounded_in_size() -> TestResult {
    let mut events = sse_stream(
        chunks(vec![
            Ok(b"id: 7\nretry: 10\ndata: {\"a\":"),
            Ok(b"1}\n\nevent: message\ndata: two\n\n"),
        ]),
        64,
    );
    let first = events.next().await.ok_or("no first event")??;
    assert_eq!(
        (first.data.as_deref(), first.id, first.retry),
        (Some("{\"a\":1}"), None, None),
        "ids are dropped so a broken request stream is never resumed"
    );
    let second = events.next().await.ok_or("no second event")??;
    assert_eq!(
        (second.event.as_deref(), second.data.as_deref()),
        (Some("message"), Some("two"))
    );
    assert!(events.next().await.is_none());

    let mut oversized = sse_stream(
        chunks(vec![Ok(b"data: 0123456789"), Ok(b"0123456789\n\n")]),
        16,
    );
    let error = oversized
        .next()
        .await
        .ok_or("no error")?
        .err()
        .ok_or("accepted")?;
    assert!(
        matches!(&error, SseError::Body(body) if matches!(body.downcast_ref::<McpHttpError>(), Some(McpHttpError::Sse(_)))),
        "{error}"
    );
    assert!(oversized.next().await.is_none());

    let failure = Failure::for_classifier_test(
        Phase::ResponseBody,
        TransmissionFact::MayHaveTransmitted,
        ErrorKind::Io,
    );
    let mut broken = sse_stream(chunks(vec![Ok(b"data: one\n\n"), Err(failure)]), 64);
    assert!(broken.next().await.ok_or("no event")?.is_ok());
    assert!(broken.next().await.ok_or("no error")?.is_err());
    assert!(broken.next().await.is_none());
    Ok(())
}

fn sent(error: McpHttpError) -> ServiceError {
    ServiceError::TransportSend(DynamicTransportError::from_parts(
        "streamable-http",
        TypeId::of::<PlakousStreamableClient>(),
        Box::new(StreamableHttpError::Client(error)),
    ))
}

#[test]
fn http_failures_classify_by_what_the_server_can_have_done() -> TestResult {
    let failure = |transmission, kind| {
        McpHttpError::Transport(Failure::for_classifier_test(
            Phase::Connect,
            transmission,
            kind,
        ))
    };
    for (error, expected) in [
        (
            McpHttpError::Status(StatusCode::UNAUTHORIZED),
            HttpRefusal::Credentials(StatusCode::UNAUTHORIZED),
        ),
        (
            McpHttpError::Status(StatusCode::FORBIDDEN),
            HttpRefusal::Credentials(StatusCode::FORBIDDEN),
        ),
        (
            McpHttpError::Status(StatusCode::BAD_GATEWAY),
            HttpRefusal::Unknown,
        ),
        (
            failure(TransmissionFact::NotStarted, ErrorKind::Io),
            HttpRefusal::NotSent,
        ),
        (
            failure(TransmissionFact::NotStarted, ErrorKind::AddressRefused),
            HttpRefusal::NotSent,
        ),
        (
            failure(TransmissionFact::MayHaveTransmitted, ErrorKind::Io),
            HttpRefusal::Unknown,
        ),
        (McpHttpError::Request, HttpRefusal::NotSent),
    ] {
        assert_eq!(classify(&sent(error)), Some(expected));
    }
    assert_eq!(classify(&ServiceError::TransportClosed), None);
    let text = credentials_text(&McpServerName::new("remote")?, StatusCode::UNAUTHORIZED);
    assert!(
        text.contains("HTTP 401") && text.contains("bearer token"),
        "{text}"
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTP error correlation and content-type matrix"
)]
async fn http_400_accepts_only_a_correlated_json_rpc_error() -> TestResult {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    for (status, content_type, session) in [
        (400, "application/json", None),
        (500, "application/json", None),
        (400, "Application/JSON; charset=utf-8", None),
        (400, "text/plain", None),
        (404, "application/json", Some("existing-session")),
        (404, "text/plain", Some("existing-session")),
    ] {
        for (reply, definite) in [
            (
                serde_json::json!({"jsonrpc":"2.0","id":42,"error":{"code":-32021,"message":"elicitation capability missing"}}),
                true,
            ),
            (
                serde_json::json!({"jsonrpc":"2.0","id":43,"error":{"code":-32021,"message":"wrong request"}}),
                false,
            ),
            (
                serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32021,"message":"uncorrelated"}}),
                false,
            ),
            (
                serde_json::json!({"jsonrpc":"2.0","id":42,"result":{}}),
                false,
            ),
            (
                serde_json::json!({"jsonrpc":"2.0","error":{"code":-32021,"message":"missing id"}}),
                false,
            ),
            (
                serde_json::json!({"jsonrpc":"2.0","id":42,"result":{},"error":{"code":-32021,"message":"ambiguous envelope"}}),
                false,
            ),
            (serde_json::json!({"error":"malformed"}), false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("http://{}/mcp", listener.local_addr()?);
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await?;
                let mut request = Vec::new();
                loop {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await?;
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&request);
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .map(str::to_owned)
                    })
                    .ok_or_else(|| std::io::Error::other("missing length"))?
                    .parse()
                    .map_err(std::io::Error::other)?;
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await?;
                let reply = reply.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await
            });
            let client = McpHttpEndpoint::new(&url)?.client(
                &McpServerName::new("remote")?,
                &Client::new(Limits::default())?,
                Duration::from_secs(5),
            )?;
            let message = serde_json::from_value(serde_json::json!({
                "jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"rpc_error","arguments":{}}
            }))?;
            let answer = client
                .post_message(
                    url.into(),
                    message,
                    session.map(Into::into),
                    None,
                    HashMap::new(),
                )
                .await;
            if definite && content_type != "text/plain" {
                let StreamableHttpPostResponse::Json(ServerJsonRpcMessage::Error(error), _) =
                    answer?
                else {
                    return Err("expected correlated JSON-RPC error".into());
                };
                assert_eq!(error.error.message, "elicitation capability missing");
            } else if status == 404 && session.is_some() {
                assert!(matches!(answer, Err(StreamableHttpError::SessionExpired)));
            } else {
                assert!(
                    answer.is_err(),
                    "uncertain HTTP response must remain unknown"
                );
            }
            server.await??;
        }
    }
    Ok(())
}
