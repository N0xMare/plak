use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use plakous_http::{Bytes, HeaderMap, HeaderValue, Limits, Method, Origin, Request as HttpRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use plakous_attempt::Jitter;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// A 1x1 PNG header: signature, IHDR length and type, width 1, height 1.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

fn source(bytes: &[u8]) -> ResponsesImageSource {
    ResponsesImageSource {
        media_type: "image/png".to_owned(),
        bytes: bytes.to_vec(),
    }
}

fn generation(flavor: ResponsesImageFlavor) -> ResponsesImageRequest {
    ResponsesImageRequest::new("image-model", flavor, "a red fox")
}

#[test]
fn openai_and_codex_bodies_are_exact() -> TestResult {
    for flavor in [ResponsesImageFlavor::OpenAi, ResponsesImageFlavor::Codex] {
        let body = generation(flavor)
            .with_size(Some("1024x1024".to_owned()))
            .body()?;
        assert_eq!(
            String::from_utf8(body)?,
            r#"{"model":"image-model","n":1,"prompt":"a red fox","size":"1024x1024"}"#
        );
        let edit = generation(flavor)
            .with_edit_from(vec![source(b"ab"), source(b"c")])
            .body()?;
        assert_eq!(
            String::from_utf8(edit)?,
            concat!(
                r#"{"images":[{"image_url":"data:image/png;base64,YWI="},"#,
                r#"{"image_url":"data:image/png;base64,Yw=="}],"#,
                r#""model":"image-model","n":1,"prompt":"a red fox"}"#
            )
        );
    }
    Ok(())
}

#[test]
fn xai_bodies_are_exact() -> TestResult {
    let body = generation(ResponsesImageFlavor::Xai).body()?;
    assert_eq!(
        String::from_utf8(body)?,
        r#"{"model":"image-model","n":1,"prompt":"a red fox","response_format":"b64_json"}"#
    );
    let edit = generation(ResponsesImageFlavor::Xai)
        .with_edit_from(vec![source(b"ab")])
        .body()?;
    assert_eq!(
        String::from_utf8(edit)?,
        concat!(
            r#"{"image":{"type":"image_url","url":"data:image/png;base64,YWI="},"#,
            r#""model":"image-model","n":1,"prompt":"a red fox","response_format":"b64_json"}"#
        )
    );
    Ok(())
}

#[test]
fn requests_a_route_cannot_express_are_refused_before_dispatch() {
    let refused = [
        generation(ResponsesImageFlavor::Xai).with_size(Some("1024x1024".to_owned())),
        generation(ResponsesImageFlavor::Xai).with_edit_from(vec![source(b"a"), source(b"b")]),
        generation(ResponsesImageFlavor::Codex).with_edit_from(vec![source(b"a"); 6]),
    ];
    for request in refused {
        assert_eq!(
            request.body().err().map(|error| error.kind()),
            Some(ResponsesRetrievalErrorKind::InvalidRequest),
            "{request:?}"
        );
    }
    assert!(
        generation(ResponsesImageFlavor::Codex)
            .with_edit_from(vec![source(b"a"); 5])
            .body()
            .is_ok()
    );
}

#[test]
fn responses_decode_inline_base64_and_data_urls_only() -> TestResult {
    let encoded = STANDARD.encode(PNG);
    let b64 = json!({"created": 1, "data": [{"b64_json": encoded}], "usage": {"total_tokens": 9}});
    assert_eq!(decode_responses_image(&serde_json::to_vec(&b64)?)?, PNG);
    let data_url = json!({"data": [{"url": format!("data:image/png;base64,{encoded}")}]});
    assert_eq!(
        decode_responses_image(&serde_json::to_vec(&data_url)?)?,
        PNG
    );
    let refused = [
        (
            json!({"data": [{"url": "https://imgen.example/a.png"}]}),
            "the provider returned an image URL instead of the image bytes",
        ),
        (
            json!({"data": [{"b64_json": encoded}, {"b64_json": encoded}]}),
            "the images response does not carry exactly one image",
        ),
        (
            json!({"data": []}),
            "the images response does not carry exactly one image",
        ),
        (
            json!({"data": [{"b64_json": "%%%"}]}),
            "the generated image is not valid base64",
        ),
        (
            json!({"data": [{"revised_prompt": "x"}]}),
            "the images response carries no image",
        ),
        (json!({"error": "x"}), "the images response has no data"),
    ];
    for (body, message) in refused {
        let error = decode_responses_image(&serde_json::to_vec(&body)?)
            .err()
            .ok_or("decoded")?;
        assert_eq!(error.kind(), ResponsesRetrievalErrorKind::InvalidResponse);
        assert_eq!(error.message(), message);
    }
    Ok(())
}

fn http_response(status: u16, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn image_response() -> Vec<u8> {
    let body = json!({"data": [{"b64_json": STANDARD.encode(PNG)}]}).to_string();
    http_response(200, "", &body)
}

async fn read_body(stream: &mut tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(Vec::new());
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buffer.len() >= end + 4 + length {
            return Ok(buffer[end + 4..end + 4 + length].to_vec());
        }
    }
}

struct Peer {
    origin: Origin,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

async fn peer(responses: Vec<Vec<u8>>) -> TestResult<Peer> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let origin = Origin::loopback_http(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        listener.local_addr()?.port(),
    )?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    tokio::spawn(async move {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            if let Ok(body) = read_body(&mut stream).await
                && let Ok(mut requests) = recorded.lock()
            {
                requests.push(body);
            }
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        }
    });
    Ok(Peer { origin, requests })
}

/// Sends to the origins in order, then repeats the last; records every
/// authorized endpoint.
struct Fixed {
    origins: Vec<Origin>,
    authorized: AtomicUsize,
    endpoints: Mutex<Vec<ResponsesImageEndpoint>>,
}

impl ResponsesImageTransport for Fixed {
    fn authorize<'a>(
        &'a self,
        endpoint: ResponsesImageEndpoint,
        body: &'a [u8],
    ) -> ResponsesImageAuthorization<'a> {
        Box::pin(async move {
            let index = self.authorized.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut endpoints) = self.endpoints.lock() {
                endpoints.push(endpoint);
            }
            let origin = self.origins[index.min(self.origins.len() - 1)].clone();
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            HttpRequest::new(
                origin,
                Method::POST,
                "/v1/images/generations",
                headers,
                Bytes::copy_from_slice(body),
                &Limits::default(),
            )
            .map(Into::into)
            .map_err(|_| invalid_request("fixture request"))
        })
    }
}

fn fixed(origins: Vec<Origin>) -> Fixed {
    Fixed {
        origins,
        authorized: AtomicUsize::new(0),
        endpoints: Mutex::new(Vec::new()),
    }
}

async fn run(
    transport: &impl ResponsesImageTransport,
    request: &ResponsesImageRequest,
) -> Result<Vec<u8>, ResponsesRetrievalError> {
    let client =
        Client::new_http1(Limits::default()).map_err(|_| invalid_request("fixture client"))?;
    run_responses_image(
        &client,
        transport,
        request,
        &CancellationToken::new(),
        ResponsesRetrievalLimits {
            exchange_timeout: Duration::from_secs(10),
            jitter: Some(Jitter::SHORTEST),
        },
    )
    .await
}

#[tokio::test]
async fn one_request_sends_the_exact_body_once_to_its_endpoint() -> TestResult {
    let peer = peer(vec![image_response(), image_response()]).await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let request = generation(ResponsesImageFlavor::OpenAi);
    assert_eq!(run(&transport, &request).await?, PNG);
    let edit = generation(ResponsesImageFlavor::OpenAi).with_edit_from(vec![source(PNG)]);
    assert_eq!(run(&transport, &edit).await?, PNG);
    let requests = peer.requests.lock().map_err(|_| "poisoned")?.clone();
    assert_eq!(requests, [request.body()?, edit.body()?]);
    let endpoints = transport.endpoints.lock().map_err(|_| "poisoned")?.clone();
    assert_eq!(
        endpoints,
        [
            ResponsesImageEndpoint::Generations,
            ResponsesImageEndpoint::Edits
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_connect_failure_and_a_rate_limit_are_retried() -> TestResult {
    let closed = {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), port)?
    };
    let peer = peer(vec![
        http_response(429, "retry-after: 0\r\n", "{}"),
        image_response(),
    ])
    .await?;
    let transport = fixed(vec![closed, peer.origin.clone()]);
    assert_eq!(
        run(&transport, &generation(ResponsesImageFlavor::Xai)).await?,
        PNG
    );
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
async fn a_refusal_or_server_error_is_never_sent_again() -> TestResult {
    let peer = peer(vec![
        http_response(
            400,
            "",
            r#"{"error":{"type":"invalid_request_error","message":"prompt rejected by safety system"}}"#,
        ),
        http_response(500, "", "{}"),
        image_response(),
    ])
    .await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let request = generation(ResponsesImageFlavor::OpenAi);
    let error = run(&transport, &request)
        .await
        .err()
        .ok_or("400 succeeded")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Rejected);
    assert_eq!(error.status(), Some(400));
    assert_eq!(
        error.message(),
        "the provider refused the side request (HTTP 400, invalid_request_error: prompt rejected by safety system)"
    );
    let error = run(&transport, &request)
        .await
        .err()
        .ok_or("500 succeeded")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unavailable);
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn a_401_without_credential_renewal_is_not_sent_again() -> TestResult {
    let peer = peer(vec![http_response(401, "", "{}"), image_response()]).await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let error = run(&transport, &generation(ResponsesImageFlavor::OpenAi))
        .await
        .err()
        .ok_or("401 was renewed")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unauthorized);
    assert_eq!(error.status(), Some(401));
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn a_possible_paid_image_is_never_reissued_after_the_peer_reads_its_body() -> TestResult {
    let peer = peer(vec![Vec::new(), image_response()]).await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let error = run(&transport, &generation(ResponsesImageFlavor::OpenAi))
        .await
        .err()
        .ok_or("ambiguous image was replayed")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unavailable);
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 1);
    assert_eq!(peer.requests.lock().map_err(|_| "requests lock")?.len(), 1);
    Ok(())
}

const WRAPPED_OVERLOAD_BODY: &str = r#"{"error":{"message":"authentication failed (status=503): {\"error\":\"Too many concurrent requests\"}","type":"authentication_error","param":null,"code":null}}"#;

fn wrapped_overload() -> Vec<u8> {
    http_response(401, "retry-after: 0\r\n", WRAPPED_OVERLOAD_BODY)
}

struct OptIn {
    inner: Fixed,
    opted_in: bool,
    renew: bool,
    refreshes: AtomicUsize,
}

impl ResponsesImageTransport for OptIn {
    fn authorize<'a>(
        &'a self,
        endpoint: ResponsesImageEndpoint,
        body: &'a [u8],
    ) -> ResponsesImageAuthorization<'a> {
        self.inner.authorize(endpoint, body)
    }
    fn credential_rejected(&self) -> ResponsesCredentialRefresh<'_> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(self.renew))
    }
    fn retry_wrapped_overload(&self) -> bool {
        self.opted_in
    }
}

fn opt_in(origin: Origin, opted_in: bool, renew: bool) -> OptIn {
    OptIn {
        inner: fixed(vec![origin]),
        opted_in,
        renew,
        refreshes: AtomicUsize::new(0),
    }
}

#[tokio::test]
async fn a_wrapped_overload_401_retries_as_its_503_without_a_refresh() -> TestResult {
    let peer = peer(vec![wrapped_overload(), image_response()]).await?;
    let transport = opt_in(peer.origin.clone(), true, true);
    assert_eq!(
        run(&transport, &generation(ResponsesImageFlavor::Codex)).await?,
        PNG
    );
    assert_eq!(transport.inner.authorized.load(Ordering::SeqCst), 2);
    assert_eq!(transport.refreshes.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn exhausted_wrapped_overloads_end_unavailable_without_a_refresh() -> TestResult {
    let peer = peer(vec![wrapped_overload(); 4]).await?;
    let transport = opt_in(peer.origin.clone(), true, true);
    let error = run(&transport, &generation(ResponsesImageFlavor::Codex))
        .await
        .err()
        .ok_or("overload succeeded")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unavailable);
    assert_eq!(error.status(), Some(401));
    assert_eq!(error.physical_requests(), 4);
    assert!(
        error
            .message()
            .contains("HTTP 401 wrapping upstream HTTP 503"),
        "{}",
        error.message()
    );
    assert_eq!(transport.inner.authorized.load(Ordering::SeqCst), 4);
    assert_eq!(transport.refreshes.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn an_opted_in_image_still_does_not_retry_a_plain_503() -> TestResult {
    let peer = peer(vec![
        http_response(503, "retry-after: 0\r\n", "{}"),
        image_response(),
    ])
    .await?;
    let transport = opt_in(peer.origin.clone(), true, true);
    let error = run(&transport, &generation(ResponsesImageFlavor::Codex))
        .await
        .err()
        .ok_or("503 was retried")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unavailable);
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(transport.inner.authorized.load(Ordering::SeqCst), 1);
    assert_eq!(transport.refreshes.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn other_401s_and_routes_without_the_opt_in_stay_credential_rejections() -> TestResult {
    let cases: [(&str, bool, &str); 7] = [
        (
            "exact body without the opt-in",
            false,
            WRAPPED_OVERLOAD_BODY,
        ),
        ("no body", true, ""),
        (
            "unparseable body",
            true,
            "authentication failed (status=503)",
        ),
        (
            "plain authentication_error",
            true,
            r#"{"error":{"message":"token expired","type":"authentication_error"}}"#,
        ),
        (
            "other wrapped status",
            true,
            r#"{"error":{"message":"authentication failed (status=401): {\"error\":\"Too many concurrent requests\"}","type":"authentication_error"}}"#,
        ),
        (
            "other upstream 503 reason",
            true,
            r#"{"error":{"message":"authentication failed (status=503): {\"error\":\"upstream unavailable\"}","type":"authentication_error"}}"#,
        ),
        (
            "other error type",
            true,
            r#"{"error":{"message":"authentication failed (status=503): {\"error\":\"Too many concurrent requests\"}","type":"invalid_request_error"}}"#,
        ),
    ];
    let request = generation(ResponsesImageFlavor::Codex);
    for (label, opted_in, body) in cases {
        let unauthorized = http_response(401, "retry-after: 0\r\n", body);
        let refused = peer(vec![unauthorized.clone()]).await?;
        let transport = opt_in(refused.origin.clone(), opted_in, false);
        let error = run(&transport, &request)
            .await
            .err()
            .ok_or_else(|| format!("{label}: a 401 without renewal succeeded"))?;
        assert_eq!(
            error.kind(),
            ResponsesRetrievalErrorKind::Unauthorized,
            "{label}"
        );
        assert_eq!(error.status(), Some(401), "{label}");
        assert_eq!(error.physical_requests(), 1, "{label}");
        assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1, "{label}");
        assert_eq!(
            transport.inner.authorized.load(Ordering::SeqCst),
            1,
            "{label}"
        );

        let renewed = peer(vec![unauthorized, image_response()]).await?;
        let transport = opt_in(renewed.origin.clone(), opted_in, true);
        assert_eq!(run(&transport, &request).await?, PNG, "{label}");
        assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1, "{label}");
        assert_eq!(
            transport.inner.authorized.load(Ordering::SeqCst),
            2,
            "{label}"
        );
    }
    Ok(())
}

/// Claims a 426 as a contract refusal and renews any 401.
struct ContractGated {
    inner: OptIn,
}

impl ResponsesImageTransport for ContractGated {
    fn authorize<'a>(
        &'a self,
        endpoint: ResponsesImageEndpoint,
        body: &'a [u8],
    ) -> ResponsesImageAuthorization<'a> {
        self.inner.authorize(endpoint, body)
    }
    fn credential_rejected(&self) -> ResponsesCredentialRefresh<'_> {
        self.inner.credential_rejected()
    }
    fn contract_refusal(&self, status: u16, _: &[u8]) -> Option<String> {
        (status == 426).then(|| "fixture contract is out of date; install a newer build".to_owned())
    }
}

#[tokio::test]
async fn an_image_contract_refusal_is_forwarded_as_the_routes_remedy() -> TestResult {
    let peer = peer(vec![
        http_response(
            426,
            "",
            r#"{"error":"please run the vendor update command"}"#,
        ),
        image_response(),
    ])
    .await?;
    let transport = ContractGated {
        inner: opt_in(peer.origin.clone(), false, true),
    };
    let error = run(&transport, &generation(ResponsesImageFlavor::Xai))
        .await
        .err()
        .ok_or("a contract refusal succeeded")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Rejected);
    assert_eq!(error.status(), Some(426));
    assert_eq!(
        error.message(),
        "fixture contract is out of date; install a newer build"
    );
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(transport.inner.inner.authorized.load(Ordering::SeqCst), 1);
    assert_eq!(transport.inner.refreshes.load(Ordering::SeqCst), 0);
    Ok(())
}
