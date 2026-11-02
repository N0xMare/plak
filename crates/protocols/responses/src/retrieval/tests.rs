#![allow(
    clippy::needless_pass_by_value,
    clippy::format_collect,
    reason = "fixture builders take owned JSON for readability"
)]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use plakous_http::{Bytes, HeaderMap, HeaderValue, Limits, Method, Origin};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use plakous_attempt::Jitter;
use plakous_http::{Failure as HttpFailure, Request as HttpRequest};
use std::time::Duration;

/// A named change to an allowlisted body.
type Case = (&'static str, fn(&mut Value));

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn web_search(allowed: &[&str], blocked: &[&str]) -> ResponsesRetrievalQuery {
    ResponsesRetrievalQuery::WebSearch {
        query: "rust 1.99".to_owned(),
        allowed_domains: allowed.iter().map(|domain| (*domain).to_owned()).collect(),
        blocked_domains: blocked.iter().map(|domain| (*domain).to_owned()).collect(),
    }
}

fn openai_search() -> ResponsesRetrievalRequest {
    ResponsesRetrievalRequest::new(
        "gpt-5.5",
        ResponsesRetrievalFlavor::OpenAi,
        web_search(&["rust-lang.org"], &[]),
    )
    .with_effort(Some("low"))
}

fn post_search() -> ResponsesRetrievalRequest {
    ResponsesRetrievalRequest::new(
        "grok-4.6",
        ResponsesRetrievalFlavor::Xai,
        ResponsesRetrievalQuery::PostSearch {
            query: "rust 1.99".to_owned(),
            handles: vec!["rustlang".to_owned()],
            from_date: Some("2026-10-01".to_owned()),
            to_date: None,
        },
    )
}

#[test]
fn the_openai_web_search_body_is_cost_only() -> TestResult {
    let body = openai_search().body()?;
    assert_eq!(
        classify_responses_retrieval_body(&body),
        ReplaySafety::CostOnly
    );
    Ok(())
}

#[test]
fn the_xai_web_and_post_search_bodies_are_cost_only() -> TestResult {
    let web = ResponsesRetrievalRequest::new(
        "grok-4.6",
        ResponsesRetrievalFlavor::Xai,
        web_search(&[], &["example.com"]),
    )
    .body()?;
    let posts = post_search().body()?;
    for body in [web, posts] {
        assert_eq!(
            classify_responses_retrieval_body(&body),
            ReplaySafety::CostOnly
        );
    }
    Ok(())
}

#[test]
fn queries_a_route_family_cannot_express_are_refused_before_dispatch() {
    let refused = [
        ResponsesRetrievalRequest::new(
            "m",
            ResponsesRetrievalFlavor::OpenAi,
            web_search(&[], &["example.com"]),
        ),
        ResponsesRetrievalRequest::new(
            "m",
            ResponsesRetrievalFlavor::Xai,
            web_search(&["a.example"], &["b.example"]),
        ),
        ResponsesRetrievalRequest::new(
            "m",
            ResponsesRetrievalFlavor::OpenAi,
            post_search().query().clone(),
        ),
    ];
    for request in refused {
        assert_eq!(
            request.body().err().map(|error| error.kind()),
            Some(ResponsesRetrievalErrorKind::InvalidRequest)
        );
    }
}

#[test]
fn only_allowlisted_one_tool_bodies_are_cost_only() -> TestResult {
    let body: Value = serde_json::from_slice(&openai_search().body()?)?;
    let cases: [Case; 8] = [
        ("tool type", |body| {
            body["tools"][0]["type"] = json!("code_interpreter");
        }),
        ("tool field", |body| {
            body["tools"][0]["user_location"] = json!({"type": "approximate"});
        }),
        ("filter field", |body| {
            body["tools"][0]["filters"] = json!({"blocked": ["x"]});
        }),
        ("two tools", |body| {
            body["tools"] = json!([{"type": "web_search"}, {"type": "web_search"}]);
        }),
        ("store", |body| {
            body["store"] = json!(true);
        }),
        ("tool choice", |body| {
            body["tool_choice"] = json!("auto");
        }),
        ("include", |body| {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }),
        ("top-level field", |body| {
            body["previous_response_id"] = json!("resp_1");
        }),
    ];
    for (case, change) in cases {
        let mut changed = body.clone();
        change(&mut changed);
        assert_eq!(
            classify_responses_retrieval_body(&serde_json::to_vec(&changed)?),
            ReplaySafety::ServerEffect,
            "{case}"
        );
    }
    let mut body = body;
    body["tools"][0]["type"] = json!("web_search_preview");
    assert_eq!(
        classify_responses_retrieval_body(&serde_json::to_vec(&body)?),
        ReplaySafety::ServerEffect
    );
    Ok(())
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap_or("")
            )
        })
        .collect()
}

fn completed(output: Value) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": output,
            "usage": {"input_tokens": 120, "output_tokens": 40, "total_tokens": 160},
        },
    })
}

fn search_call(status: &str, sources: Value) -> Value {
    json!({
        "type": "web_search_call",
        "id": "ws_1",
        "status": status,
        "action": {"type": "search", "query": "rust 1.99", "sources": sources},
    })
}

fn answer(text: &str, annotations: Value) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": annotations}],
    })
}

fn answered_search() -> String {
    sse(&[
        json!({"type": "response.created", "response": {"status": "in_progress"}}),
        json!({"type": "response.web_search_call.searching", "item_id": "ws_1"}),
        json!({"type": "response.output_text.delta", "delta": "Rust 1.99"}),
        completed(json!([
            {"type": "reasoning", "id": "rs_1", "summary": []},
            search_call("completed", json!([
                {"type": "url", "url": "https://blog.rust-lang.org/a"},
                {"type": "url", "url": "https://example.com/b"},
            ])),
            answer("Rust 1.99 shipped.", json!([
                {"type": "url_citation", "url": "https://blog.rust-lang.org/a", "title": "Rust 1.99",
                 "start_index": 0, "end_index": 9},
                {"type": "url_citation", "url": "https://c.example/", "title": ""},
            ])),
        ])),
    ])
}

fn collect(body: &str, call_item: &str) -> Result<ResponsesRetrievalOutcome, Broken> {
    let mut collector = Collector::default();
    collector.push(body.as_bytes())?;
    let terminal = collector.finish()?;
    assemble(&terminal, call_item).map_err(Broken::Fatal)
}

fn fatal_kind(
    result: Result<ResponsesRetrievalOutcome, Broken>,
) -> Option<(bool, ResponsesRetrievalErrorKind)> {
    match result {
        Ok(_) => None,
        Err(Broken::Transient(error)) => Some((true, error.kind())),
        Err(Broken::Fatal(error)) => Some((false, error.kind())),
    }
}

#[test]
fn search_stream_yields_the_answer_and_unique_sources() -> TestResult {
    let outcome = collect(&answered_search(), "web_search_call").map_err(|_| "broken")?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(
        outcome.sources,
        [
            ResponsesRetrievalSource {
                url: "https://blog.rust-lang.org/a".to_owned(),
                title: None,
            },
            ResponsesRetrievalSource {
                url: "https://example.com/b".to_owned(),
                title: None,
            },
            ResponsesRetrievalSource {
                url: "https://c.example/".to_owned(),
                title: None,
            },
        ]
    );
    assert_eq!(
        outcome.usage,
        ResponsesRetrievalUsage {
            input_tokens: 120,
            output_tokens: 40,
            hosted_requests: 1,
        }
    );
    Ok(())
}

#[test]
fn an_answer_without_sources_is_kept() -> TestResult {
    let body = sse(&[completed(json!([
        search_call("completed", json!([])),
        answer("Nothing found.", json!([])),
    ]))]);
    let outcome = collect(&body, "web_search_call").map_err(|_| "broken")?;
    assert_eq!(outcome.text, "Nothing found.");
    assert_eq!(outcome.sources, []);
    Ok(())
}

#[test]
fn a_failed_tool_call_without_an_answer_is_an_error() {
    let body = sse(&[completed(json!([search_call("failed", json!([]))]))]);
    assert_eq!(
        fatal_kind(collect(&body, "web_search_call")),
        Some((false, ResponsesRetrievalErrorKind::Rejected))
    );
}

/// The item shapes of a live xAI `x_search` side response: each server-side X
/// tool it ran is a `custom_tool_call`, and the citations are on the answer.
#[test]
fn xai_x_tool_calls_count_and_the_answer_cites_the_posts() -> TestResult {
    let body = sse(&[completed(json!([
        {"type": "reasoning", "id": "rs_1", "status": "completed",
         "summary": [{"type": "summary_text", "text": "Searching X."}]},
        {"type": "custom_tool_call", "id": "ctc_1", "call_id": "xs_call-1-0",
         "name": "x_keyword_search", "status": "completed",
         "input": "{\"query\":\"from:rustlang\",\"limit\":\"5\",\"mode\":\"Latest\"}"},
        {"type": "reasoning", "id": "tco_1", "status": "completed", "summary": []},
        answer("Rust 1.99 is out. [[1]](https://x.com/rustlang/status/1)", json!([
            {"type": "url_citation", "url": "https://x.com/rustlang/status/1", "title": "1"},
        ])),
    ]))]);
    let outcome = collect(&body, "x_search_call").map_err(|_| "broken")?;
    assert_eq!(outcome.sources.len(), 1);
    assert_eq!(outcome.sources[0].url, "https://x.com/rustlang/status/1");
    assert_eq!(outcome.usage.hosted_requests, 1);
    let web = sse(&[completed(json!([
        {"type": "custom_tool_call", "id": "ctc_1", "call_id": "c", "name": "x_keyword_search",
         "status": "completed", "input": "{}"},
        answer("x", json!([])),
    ]))]);
    assert_eq!(
        fatal_kind(collect(&web, "web_search_call")),
        Some((false, ResponsesRetrievalErrorKind::InvalidResponse))
    );
    Ok(())
}

#[test]
fn the_side_grammar_refuses_what_its_body_did_not_offer() {
    for item in [
        json!({"type": "function_call", "name": "exec", "arguments": "{}", "call_id": "c"}),
        json!({"type": "code_interpreter_call", "id": "ci"}),
        search_call("completed", json!([])),
    ] {
        let body = sse(&[completed(json!([item, answer("x", json!([]))]))]);
        assert_eq!(
            fatal_kind(collect(&body, "x_search_call")),
            Some((false, ResponsesRetrievalErrorKind::InvalidResponse))
        );
    }
    let foreign = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
    assert_eq!(
        fatal_kind(collect(foreign, "web_search_call")),
        Some((false, ResponsesRetrievalErrorKind::InvalidResponse))
    );
}

#[test]
fn a_stream_cut_before_its_terminal_response_is_transient() {
    let cut = sse(&[json!({"type": "response.created", "response": {}})]);
    assert_eq!(
        fatal_kind(collect(&cut, "web_search_call")),
        Some((true, ResponsesRetrievalErrorKind::Unavailable))
    );
    let failed = sse(&[json!({"type": "response.failed",
        "response": {"error": {"code": "server_error", "message": "m"}}})]);
    assert_eq!(
        fatal_kind(collect(&failed, "web_search_call")),
        Some((true, ResponsesRetrievalErrorKind::Unavailable))
    );
    let refused = sse(&[json!({"type": "response.failed",
        "response": {"error": {"code": "invalid_prompt", "message": "m"}}})]);
    assert_eq!(
        fatal_kind(collect(&refused, "web_search_call")),
        Some((false, ResponsesRetrievalErrorKind::Rejected))
    );
}

struct Peer {
    origin: Origin,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

fn http_response(status: u16, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn sse_response(body: &str) -> Vec<u8> {
    http_response(200, "content-type: text/event-stream\r\n", body)
}

async fn read_body(stream: &mut tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
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

async fn peer(responses: Vec<Vec<u8>>) -> TestResult<Peer> {
    listening_peer(responses, None).await
}

/// Like [`peer`], and signals after the first response bytes are written.
async fn peer_signaling_first_write(
    responses: Vec<Vec<u8>>,
) -> TestResult<(Peer, tokio::sync::oneshot::Receiver<()>)> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    Ok((listening_peer(responses, Some(sender)).await?, receiver))
}

async fn listening_peer(
    responses: Vec<Vec<u8>>,
    first_write: Option<tokio::sync::oneshot::Sender<()>>,
) -> TestResult<Peer> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let origin = Origin::loopback_http(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        listener.local_addr()?.port(),
    )?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    tokio::spawn(async move {
        let mut first_write = first_write;
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
            if let Some(signal) = first_write.take() {
                let _ = stream.flush().await;
                let _ = signal.send(());
            }
            let _ = stream.shutdown().await;
        }
    });
    Ok(Peer { origin, requests })
}

/// Sends to the origins in order, then repeats the last; counts
/// authorizations, which are the physical requests.
struct Fixed {
    origins: Vec<Origin>,
    authorized: AtomicUsize,
}

impl ResponsesRetrievalTransport for Fixed {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        Box::pin(async move {
            let index = self.authorized.fetch_add(1, Ordering::SeqCst);
            let origin = self.origins[index.min(self.origins.len() - 1)].clone();
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            HttpRequest::new(
                origin,
                Method::POST,
                "/v1/responses",
                headers,
                Bytes::copy_from_slice(body),
                &Limits::default(),
            )
            .map(Into::into)
            .map_err(|_| ResponsesRetrievalError::invalid_request("fixture request"))
        })
    }
}

fn fixed(origins: Vec<Origin>) -> Fixed {
    Fixed {
        origins,
        authorized: AtomicUsize::new(0),
    }
}

async fn run(
    transport: &Fixed,
    request: &ResponsesRetrievalRequest,
) -> Result<ResponsesRetrievalOutcome, ResponsesRetrievalError> {
    let client = Client::new_http1(Limits::default())
        .map_err(|_| ResponsesRetrievalError::invalid_request("fixture client"))?;
    run_responses_retrieval(
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
async fn a_connect_failure_is_retried_on_a_fresh_authorization() -> TestResult {
    let closed = {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), port)?
    };
    let peer = peer(vec![sse_response(&answered_search())]).await?;
    let transport = fixed(vec![closed, peer.origin.clone()]);
    let outcome = run(&transport, &openai_search()).await?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn a_rate_limit_is_retried_and_a_client_error_is_not() -> TestResult {
    let peer = peer(vec![
        http_response(429, "retry-after: 0\r\n", "{}"),
        sse_response(&answered_search()),
        http_response(
            400,
            "content-type: application/json\r\n",
            r#"{"error":{"type":"invalid_request_error","code":null,"message":"tool not supported"}}"#,
        ),
        http_response(
            400,
            "content-type: application/json\r\n",
            r#"{"code":"Client specified an invalid argument","error":"x_search is not enabled"}"#,
        ),
    ])
    .await?;
    let transport = fixed(vec![peer.origin.clone()]);
    assert_eq!(
        run(&transport, &openai_search()).await?.text,
        "Rust 1.99 shipped."
    );
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 2);
    let error = run(&transport, &openai_search())
        .await
        .err()
        .ok_or("400 succeeded")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Rejected);
    assert_eq!(error.status(), Some(400));
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(
        error.message(),
        "the provider refused the side request (HTTP 400, invalid_request_error: tool not supported)"
    );
    let error = run(&transport, &post_search())
        .await
        .err()
        .ok_or("400 succeeded")?;
    assert_eq!(
        error.message(),
        "the provider refused the side request (HTTP 400, Client specified an invalid argument: x_search is not enabled)"
    );
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 4);
    Ok(())
}

/// Records what happens to each physical request's guard.
struct Recorded(Arc<Mutex<Vec<&'static str>>>);

impl ResponsesRetrievalGuard for Recorded {
    fn transport_failed(&mut self, _: HttpFailure) {
        if let Ok(mut events) = self.0.lock() {
            events.push("failed");
        }
    }
}

impl Drop for Recorded {
    fn drop(&mut self) {
        if let Ok(mut events) = self.0.lock() {
            events.push("released");
        }
    }
}

/// [`Fixed`] with a recorded guard on every physical request.
struct Guarded {
    fixed: Fixed,
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl ResponsesRetrievalTransport for Guarded {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        Box::pin(async move {
            let dispatch = self.fixed.authorize(body).await?;
            Ok(dispatch.with_guard(Recorded(Arc::clone(&self.events))))
        })
    }
}

fn limits() -> ResponsesRetrievalLimits {
    ResponsesRetrievalLimits {
        exchange_timeout: Duration::from_secs(10),
        jitter: Some(Jitter::SHORTEST),
    }
}

async fn closed_origin() -> TestResult<Origin> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(Origin::loopback_http(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
    )?)
}

#[tokio::test]
async fn a_guard_learns_of_a_connect_failure_and_is_released_at_the_response_head() -> TestResult {
    let peer = peer(vec![sse_response(&answered_search())]).await?;
    let transport = Guarded {
        fixed: fixed(vec![closed_origin().await?, peer.origin.clone()]),
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let client = Client::new_http1(Limits::default())?;
    let outcome = run_responses_retrieval(
        &client,
        &transport,
        &openai_search(),
        &CancellationToken::new(),
        limits(),
    )
    .await?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(
        *transport.events.lock().map_err(|_| "poisoned")?,
        ["failed", "released", "released"]
    );
    Ok(())
}

fn cost_only(_: &[u8]) -> ReplaySafety {
    ReplaySafety::CostOnly
}

fn server_effect(_: &[u8]) -> ReplaySafety {
    ReplaySafety::ServerEffect
}

#[tokio::test]
async fn a_json_side_request_returns_its_document_after_a_retried_rate_limit() -> TestResult {
    let document = r#"{"output":"Rust 1.99 shipped.","results":[]}"#;
    let peer = peer(vec![
        http_response(429, "retry-after: 0\r\n", "{}"),
        http_response(200, "content-type: application/json\r\n", document),
    ])
    .await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let client = Client::new_http1(Limits::default())?;
    let body = br#"{"q":"rust"}"#;
    let returned = run_responses_json_side_request(
        &client,
        &transport,
        body,
        cost_only,
        &CancellationToken::new(),
        limits(),
    )
    .await?;
    assert_eq!(returned, document.as_bytes());
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 2);
    assert_eq!(
        *peer.requests.lock().map_err(|_| "poisoned")?,
        [body.to_vec(), body.to_vec()]
    );
    Ok(())
}

#[tokio::test]
async fn a_json_side_request_outside_the_allowlist_or_over_its_bound_fails() -> TestResult {
    let oversized = "a".repeat(MAX_DOCUMENT_BYTES + 1);
    let peer = peer(vec![http_response(
        200,
        "content-type: application/json\r\n",
        &oversized,
    )])
    .await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let client = Client::new_http1(Limits::default())?;
    let refused = run_responses_json_side_request(
        &client,
        &transport,
        b"{}",
        server_effect,
        &CancellationToken::new(),
        limits(),
    )
    .await
    .err()
    .ok_or("an unclassified body was sent")?;
    assert_eq!(refused.kind(), ResponsesRetrievalErrorKind::InvalidRequest);
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 0);
    let bounded = run_responses_json_side_request(
        &client,
        &transport,
        b"{}",
        cost_only,
        &CancellationToken::new(),
        limits(),
    )
    .await
    .err()
    .ok_or("an oversized document was accepted")?;
    assert_eq!(bounded.kind(), ResponsesRetrievalErrorKind::InvalidResponse);
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 1);
    Ok(())
}

/// The exact body the Codex subscription backend sends with HTTP 401 when its
/// upstream authentication service refuses with a transient 503.
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

impl ResponsesRetrievalTransport for OptIn {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        self.inner.authorize(body)
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

async fn run_json(transport: &OptIn, body: &[u8]) -> Result<Vec<u8>, ResponsesRetrievalError> {
    let client = Client::new_http1(Limits::default())
        .map_err(|_| ResponsesRetrievalError::invalid_request("fixture client"))?;
    run_responses_json_side_request(
        &client,
        transport,
        body,
        cost_only,
        &CancellationToken::new(),
        limits(),
    )
    .await
}

#[tokio::test]
async fn a_wrapped_overload_401_retries_as_its_503_without_a_refresh() -> TestResult {
    let document = r#"{"output":"Rust 1.99 shipped."}"#;
    let peer = peer(vec![
        wrapped_overload(),
        http_response(200, "content-type: application/json\r\n", document),
    ])
    .await?;
    let transport = opt_in(peer.origin.clone(), true, true);
    let returned = run_json(&transport, b"{\"q\":\"rust\"}").await?;
    assert_eq!(returned, document.as_bytes());
    assert_eq!(transport.inner.authorized.load(Ordering::SeqCst), 2);
    assert_eq!(transport.refreshes.load(Ordering::SeqCst), 0);
    Ok(())
}

struct Marked {
    inner: OptIn,
    marks: Arc<Mutex<Vec<tokio::time::Instant>>>,
}

impl ResponsesRetrievalTransport for Marked {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        if let Ok(mut marks) = self.marks.lock() {
            marks.push(tokio::time::Instant::now());
        }
        self.inner.authorize(body)
    }
    fn credential_rejected(&self) -> ResponsesCredentialRefresh<'_> {
        self.inner.credential_rejected()
    }
    fn retry_wrapped_overload(&self) -> bool {
        self.inner.retry_wrapped_overload()
    }
}

/// Yields so a spawned side request can finish real loopback IO. The paused
/// clock stays frozen, so this does not spend a connect or exchange budget.
async fn drain_loopback() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// A nonzero `Retry-After` on the wrapped overload is the wait. Dropping the
/// advice falls back to the jittered one-second backoff, which is not five
/// seconds even at the longest sample.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_wrapped_overload_waits_out_a_nonzero_retry_after() -> TestResult {
    // Freezes auto-advance for both real connects. A paused runtime otherwise
    // jumps the connect timeout when the loopback socket is not ready in the
    // zero-timeout park, and the side request dispatches again.
    let (keep_clock, clock_held) = std::sync::mpsc::channel::<()>();
    let clock = tokio::task::spawn_blocking(move || {
        let _ = clock_held.recv();
    });
    let document = r#"{"output":"Rust 1.99 shipped."}"#;
    let (peer, wrote) = peer_signaling_first_write(vec![
        http_response(401, "retry-after: 5\r\n", WRAPPED_OVERLOAD_BODY),
        http_response(200, "content-type: application/json\r\n", document),
    ])
    .await?;
    let marks = Arc::new(Mutex::new(Vec::new()));
    let transport = Marked {
        inner: opt_in(peer.origin.clone(), true, true),
        marks: Arc::clone(&marks),
    };
    let client = Client::new_http1(Limits::default())?;
    let cancellation = CancellationToken::new();
    let run = tokio::spawn(async move {
        run_responses_json_side_request(
            &client,
            &transport,
            b"{\"q\":\"rust\"}",
            cost_only,
            &cancellation,
            ResponsesRetrievalLimits {
                exchange_timeout: Duration::from_secs(30),
                jitter: Some(Jitter::LONGEST),
            },
        )
        .await
    });
    // The peer signals after the 401 is on the socket. The spawned request
    // then reads it and parks on the retry sleep; virtual time stays put.
    wrote
        .await
        .map_err(|_| "peer dropped before writing the 401")?;
    drain_loopback().await;
    assert!(
        !run.is_finished(),
        "side request finished before its retry sleep"
    );
    assert_eq!(
        marks.lock().map_err(|_| "marks lock")?.len(),
        1,
        "the overload retried before any logical wait"
    );
    let parked = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_secs(5) - Duration::from_millis(1)).await;
    assert!(
        !run.is_finished(),
        "the overload retried before Retry-After elapsed"
    );
    assert_eq!(
        marks.lock().map_err(|_| "marks lock")?.len(),
        1,
        "the overload retried before Retry-After elapsed"
    );
    // A five-second `Retry-After` completes on the following millisecond.
    tokio::time::advance(Duration::from_millis(1)).await;
    let returned = run.await??;
    assert_eq!(returned, document.as_bytes());
    {
        let marks = marks.lock().map_err(|_| "marks lock")?;
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[1].duration_since(parked), Duration::from_secs(5));
    }
    drop(keep_clock);
    clock.await?;
    Ok(())
}

#[tokio::test]
async fn exhausted_wrapped_overloads_end_unavailable_without_a_refresh() -> TestResult {
    let peer = peer(vec![wrapped_overload(); 4]).await?;
    let transport = opt_in(peer.origin.clone(), true, true);
    let error = run_json(&transport, b"{}")
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
    for (label, opted_in, body) in cases {
        let unauthorized = http_response(401, "retry-after: 0\r\n", body);
        let refused = peer(vec![unauthorized.clone()]).await?;
        let transport = opt_in(refused.origin.clone(), opted_in, false);
        let error = run_json(&transport, b"{}")
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
        assert_eq!(
            transport.inner.authorized.load(Ordering::SeqCst),
            1,
            "{label}"
        );
        assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1, "{label}");

        let renewed = peer(vec![
            unauthorized,
            http_response(200, "content-type: application/json\r\n", r#"{"ok":true}"#),
        ])
        .await?;
        let transport = opt_in(renewed.origin.clone(), opted_in, true);
        let returned = run_json(&transport, b"{}").await?;
        assert_eq!(returned, br#"{"ok":true}"#, "{label}");
        assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1, "{label}");
        assert_eq!(
            transport.inner.authorized.load(Ordering::SeqCst),
            2,
            "{label}"
        );
    }
    Ok(())
}

const CONTRACT_REMEDY: &str = "fixture contract is out of date; install a newer build";

/// Claims every refusal whose body carries `contract-gate`, renewing any 401.
struct ContractGated {
    inner: OptIn,
}

impl ResponsesRetrievalTransport for ContractGated {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        self.inner.authorize(body)
    }
    fn credential_rejected(&self) -> ResponsesCredentialRefresh<'_> {
        self.inner.credential_rejected()
    }
    fn contract_refusal(&self, _: u16, body: &[u8]) -> Option<String> {
        body.windows(13)
            .any(|window| window == b"contract-gate")
            .then(|| CONTRACT_REMEDY.to_owned())
    }
}

async fn run_gated(transport: &ContractGated) -> TestResult<ResponsesRetrievalOutcome> {
    let client = Client::new_http1(Limits::default())?;
    Ok(run_responses_retrieval(
        &client,
        transport,
        &openai_search(),
        &CancellationToken::new(),
        limits(),
    )
    .await?)
}

async fn gated_error(transport: &ContractGated) -> TestResult<ResponsesRetrievalError> {
    let client = Client::new_http1(Limits::default())?;
    run_responses_retrieval(
        &client,
        transport,
        &openai_search(),
        &CancellationToken::new(),
        limits(),
    )
    .await
    .err()
    .ok_or_else(|| "a refused side request succeeded".into())
}

const GATED_BODY: &str = r#"{"error":"contract-gate: please run the vendor update command"}"#;

#[tokio::test]
async fn a_claimed_contract_refusal_is_final_and_replaces_the_provider_text() -> TestResult {
    for status in [426, 400, 429, 503] {
        let refused = peer(vec![
            http_response(status, "retry-after: 0\r\n", GATED_BODY),
            sse_response(&answered_search()),
        ])
        .await?;
        let transport = ContractGated {
            inner: opt_in(refused.origin.clone(), false, true),
        };
        let error = gated_error(&transport).await?;
        assert_eq!(
            error.kind(),
            ResponsesRetrievalErrorKind::Rejected,
            "{status}"
        );
        assert_eq!(error.status(), Some(status));
        assert_eq!(error.message(), CONTRACT_REMEDY);
        assert_eq!(error.physical_requests(), 1, "{status}");
        assert_eq!(transport.inner.inner.authorized.load(Ordering::SeqCst), 1);
        assert_eq!(transport.inner.refreshes.load(Ordering::SeqCst), 0);
    }
    let unclaimed = peer(vec![http_response(426, "", r#"{"error":"upgrade"}"#)]).await?;
    let transport = ContractGated {
        inner: opt_in(unclaimed.origin.clone(), false, true),
    };
    assert_eq!(
        gated_error(&transport).await?.message(),
        "the provider refused the side request (HTTP 426, error: upgrade)"
    );
    Ok(())
}

#[tokio::test]
async fn credential_refusals_are_never_offered_as_contract_refusals() -> TestResult {
    let renewed = peer(vec![
        http_response(401, "", GATED_BODY),
        sse_response(&answered_search()),
    ])
    .await?;
    let transport = ContractGated {
        inner: opt_in(renewed.origin.clone(), false, true),
    };
    assert_eq!(run_gated(&transport).await?.text, "Rust 1.99 shipped.");
    assert_eq!(transport.inner.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(transport.inner.inner.authorized.load(Ordering::SeqCst), 2);

    let forbidden = peer(vec![http_response(403, "", GATED_BODY)]).await?;
    let transport = ContractGated {
        inner: opt_in(forbidden.origin.clone(), false, true),
    };
    let error = gated_error(&transport).await?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unauthorized);
    assert_eq!(error.status(), Some(403));
    assert_ne!(error.message(), CONTRACT_REMEDY);
    assert_eq!(error.physical_requests(), 1);
    Ok(())
}
