#![allow(
    clippy::needless_pass_by_value,
    clippy::format_collect,
    reason = "fixture builders take owned JSON for readability"
)]

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use plakous_http::{Bytes, HeaderMap, HeaderValue, Limits, Method, Origin};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn search() -> ChatRetrievalRequest {
    ChatRetrievalRequest::new("vendor/model-literal", "rust 1.99")
}

#[test]
fn web_search_body_is_exact_and_cost_only() -> TestResult {
    let body = search().body()?;
    assert_eq!(
        String::from_utf8(body.clone())?,
        concat!(
            r#"{"model":"vendor/model-literal","messages":[{"role":"user","content":"#,
            r#""Search the web for: rust 1.99. Answer concisely and cite sources."}],"#,
            r#""max_tokens":4096,"provider":{"allow_fallbacks":false,"require_parameters":true},"#,
            r#""plugins":[{"id":"web","max_results":5}],"stream":true,"#,
            r#""stream_options":{"include_usage":true}}"#,
        )
    );
    assert_eq!(classify_chat_retrieval_body(&body), ReplaySafety::CostOnly);
    Ok(())
}

#[test]
fn filters_reasoning_and_routing_are_exact_and_cost_only() -> TestResult {
    let body = search()
        .with_domains(
            vec!["rust-lang.org".to_owned()],
            vec!["example.com".to_owned()],
        )
        .with_reasoning(ChatRetrievalReasoning::Lowest("minimal"))
        .with_routing(ChatRetrievalRouting {
            data_collection: Some("deny".to_owned()),
            zdr: Some(true),
        })
        .body()?;
    assert_eq!(
        String::from_utf8(body.clone())?,
        concat!(
            r#"{"model":"vendor/model-literal","messages":[{"role":"user","content":"#,
            r#""Search the web for: rust 1.99. Answer concisely and cite sources."}],"#,
            r#""max_tokens":4096,"reasoning":{"effort":"minimal","exclude":true},"#,
            r#""provider":{"allow_fallbacks":false,"require_parameters":true,"#,
            r#""data_collection":"deny","zdr":true},"#,
            r#""plugins":[{"id":"web","max_results":5,"include_domains":["rust-lang.org"],"#,
            r#""exclude_domains":["example.com"]}],"stream":true,"#,
            r#""stream_options":{"include_usage":true}}"#,
        )
    );
    assert_eq!(classify_chat_retrieval_body(&body), ReplaySafety::CostOnly);
    let disabled = search()
        .with_reasoning(ChatRetrievalReasoning::Disabled)
        .body()?;
    let value: Value = serde_json::from_slice(&disabled)?;
    assert_eq!(value["reasoning"], json!({"enabled": false}));
    Ok(())
}

#[test]
fn only_the_one_web_plugin_body_is_cost_only() -> TestResult {
    let base: Value = serde_json::from_slice(&search().body()?)?;
    let variant = |edit: &dyn Fn(&mut Value)| -> TestResult<ReplaySafety> {
        let mut body = base.clone();
        edit(&mut body);
        Ok(classify_chat_retrieval_body(&serde_json::to_vec(&body)?))
    };
    assert_eq!(variant(&|_| {})?, ReplaySafety::CostOnly);
    for edit in [
        &(|body: &mut Value| body["tools"] = json!([{"type": "function"}])) as &dyn Fn(&mut Value),
        &|body| body["plugins"] = json!([]),
        &|body| body["plugins"] = json!([{"id": "web"}, {"id": "web"}]),
        &|body| body["plugins"] = json!([{"id": "file-parser"}]),
        &|body| body["plugins"][0]["engine"] = json!("exa"),
        &|body| body["plugins"][0]["search_prompt"] = json!("x"),
        &|body| body["session_id"] = json!("s"),
        &|body| body["web_search_options"] = json!({}),
    ] {
        assert_eq!(variant(edit)?, ReplaySafety::ServerEffect);
    }
    assert_eq!(
        classify_chat_retrieval_body(b"not json"),
        ReplaySafety::ServerEffect
    );
    Ok(())
}

fn sse(chunks: &[Value]) -> String {
    let mut body = String::from(": OPENROUTER PROCESSING\n\n");
    for chunk in chunks {
        let _ = write!(body, "data: {chunk}\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": "gen-1",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": "vendor/model-literal",
        "provider": "Fixture",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish, "native_finish_reason": finish}],
    })
}

fn citation(url: &str, title: &str) -> Value {
    json!({
        "type": "url_citation",
        "url_citation": {"url": url, "title": title, "content": "snippet", "start_index": 0, "end_index": 4}
    })
}

fn usage_chunk() -> Value {
    json!({
        "id": "gen-1",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": "vendor/model-literal",
        "choices": [],
        "usage": {"prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150, "cost": 0.02},
    })
}

fn answered() -> String {
    sse(&[
        chunk(json!({"role": "assistant", "content": ""}), None),
        chunk(json!({"content": "Rust 1.99 "}), None),
        chunk(
            json!({"content": "shipped.", "annotations": [
                citation("https://blog.rust-lang.org/", "Rust Blog"),
                citation("https://www.rust-lang.org/", ""),
                citation("https://blog.rust-lang.org/", "duplicate"),
            ]}),
            None,
        ),
        chunk(json!({}), Some("stop")),
        usage_chunk(),
    ])
}

fn collect(body: &str) -> Result<ChatRetrievalOutcome, ChatRetrievalError> {
    let mut collector = Collector::default();
    let broken = |broken: Broken| match broken {
        Broken::Transient(error) | Broken::Fatal(error) => error,
    };
    for piece in body.as_bytes().chunks(7) {
        collector.push(piece).map_err(broken)?;
    }
    collector.finish().map_err(broken)
}

#[test]
fn streamed_annotations_become_unique_sources_beside_the_answer() -> TestResult {
    let outcome = collect(&answered())?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(
        outcome.sources,
        vec![
            ChatRetrievalSource {
                url: "https://blog.rust-lang.org/".to_owned(),
                title: Some("Rust Blog".to_owned()),
            },
            ChatRetrievalSource {
                url: "https://www.rust-lang.org/".to_owned(),
                title: None,
            },
        ]
    );
    assert_eq!(
        outcome.usage,
        ChatRetrievalUsage {
            input_tokens: 120,
            output_tokens: 30,
            hosted_requests: 1,
        }
    );
    Ok(())
}

#[test]
fn an_answer_without_annotations_has_no_sources() -> TestResult {
    let outcome = collect(&sse(&[
        chunk(
            json!({"role": "assistant", "content": "Nothing found."}),
            None,
        ),
        chunk(json!({}), Some("stop")),
    ]))?;
    assert_eq!(outcome.text, "Nothing found.");
    assert_eq!(outcome.sources, Vec::<ChatRetrievalSource>::new());
    Ok(())
}

#[test]
fn a_reported_search_count_replaces_the_one_search_default() -> TestResult {
    let mut usage = usage_chunk();
    usage["usage"]["server_tool_use"] = json!({"web_search_requests": 2});
    let outcome = collect(&sse(&[chunk(json!({"content": "a"}), Some("stop")), usage]))?;
    assert_eq!(outcome.usage.hosted_requests, 2);
    Ok(())
}

#[test]
fn non_streamed_message_annotations_become_sources() -> TestResult {
    let body = json!({
        "id": "gen-1",
        "object": "chat.completion",
        "choices": [{"index": 0, "finish_reason": "stop", "message": {
            "role": "assistant",
            "content": "Rust 1.99 shipped.",
            "annotations": [citation("https://blog.rust-lang.org/", "Rust Blog")]
        }}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    });
    let outcome = completed(&serde_json::to_vec(&body)?)?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(outcome.sources.len(), 1);
    assert_eq!(outcome.usage.input_tokens, 10);
    let empty = json!({"choices": [{"index": 0, "finish_reason": "stop", "message": {
        "role": "assistant", "content": "No sources.", "annotations": []
    }}]});
    assert_eq!(
        completed(&serde_json::to_vec(&empty)?)?.sources,
        Vec::<ChatRetrievalSource>::new()
    );
    Ok(())
}

#[test]
fn the_side_grammar_refuses_what_its_body_did_not_offer() {
    for refused in [
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}}]}),
            None,
        ),
        chunk(json!({"content": "a", "images": []}), None),
        chunk(json!({"annotations": [{"type": "file", "file": {}}]}), None),
        chunk(
            json!({"annotations": [{"type": "url_citation", "url_citation": {}}]}),
            None,
        ),
        json!({"choices": [{"index": 1, "delta": {"content": "second choice"}}]}),
    ] {
        let error = collect(&sse(&[refused, chunk(json!({}), Some("stop"))])).err();
        assert_eq!(
            error.map(|error| error.kind()),
            Some(ChatRetrievalErrorKind::InvalidResponse)
        );
    }
    let tool_finish = collect(&sse(&[chunk(json!({}), Some("tool_calls"))])).err();
    assert_eq!(
        tool_finish.map(|error| error.kind()),
        Some(ChatRetrievalErrorKind::InvalidResponse)
    );
}

#[test]
fn a_stream_cut_before_its_finish_reason_is_retryable() {
    let mut collector = Collector::default();
    let _ =
        collector.push(format!("data: {}\n\n", chunk(json!({"content": "a"}), None)).as_bytes());
    assert!(matches!(
        collector.finish(),
        Err(Broken::Transient(error)) if error.kind() == ChatRetrievalErrorKind::Unavailable
    ));
}

#[test]
fn a_mid_stream_error_is_transient_only_for_overload() {
    let overloaded = json!({"error": {"code": 502, "message": "upstream overloaded"}});
    let refused = json!({"error": {"code": 400, "message": "bad plugin"}});
    let mut collector = Collector::default();
    assert!(matches!(
        collector.push(format!("data: {overloaded}\n\n").as_bytes()),
        Err(Broken::Transient(_))
    ));
    let mut collector = Collector::default();
    assert!(matches!(
        collector.push(format!("data: {refused}\n\n").as_bytes()),
        Err(Broken::Fatal(error)) if error.kind() == ChatRetrievalErrorKind::Rejected
    ));
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

/// Sends to the origins in order, then repeats the last; counts
/// authorizations, which are the physical requests.
struct Fixed {
    origins: Vec<Origin>,
    authorized: AtomicUsize,
}

impl ChatRetrievalTransport for Fixed {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ChatRetrievalAuthorization<'a> {
        Box::pin(async move {
            let index = self.authorized.fetch_add(1, Ordering::SeqCst);
            let origin = self.origins[index.min(self.origins.len() - 1)].clone();
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            HttpRequest::new(
                origin,
                Method::POST,
                "/api/v1/chat/completions",
                headers,
                Bytes::copy_from_slice(body),
                &Limits::default(),
            )
            .map_err(|_| ChatRetrievalError::invalid_request("fixture request"))
        })
    }
}

fn fixed(origins: Vec<Origin>) -> Fixed {
    Fixed {
        origins,
        authorized: AtomicUsize::new(0),
    }
}

async fn run(transport: &Fixed) -> Result<ChatRetrievalOutcome, ChatRetrievalError> {
    let client = Client::new_http1(Limits::default())
        .map_err(|_| ChatRetrievalError::invalid_request("fixture client"))?;
    run_chat_retrieval(
        &client,
        transport,
        &search(),
        &CancellationToken::new(),
        ChatRetrievalLimits {
            exchange_timeout: Duration::from_secs(10),
            jitter: Some(Jitter::SHORTEST),
        },
    )
    .await
}

#[tokio::test]
async fn one_side_request_sends_the_exact_body_and_reads_its_stream() -> TestResult {
    let peer = peer(vec![sse_response(&answered())]).await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let outcome = run(&transport).await?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(outcome.sources.len(), 2);
    let requests = peer.requests.lock().map_err(|_| "poisoned")?.clone();
    assert_eq!(requests, vec![search().body()?]);
    Ok(())
}

#[tokio::test]
async fn a_non_streamed_response_is_read_whole() -> TestResult {
    let body = json!({"choices": [{"index": 0, "finish_reason": "stop", "message": {
        "role": "assistant", "content": "ok",
        "annotations": [citation("https://example.com/", "Example")]
    }}]})
    .to_string();
    let peer = peer(vec![http_response(
        200,
        "content-type: application/json\r\n",
        &body,
    )])
    .await?;
    let outcome = run(&fixed(vec![peer.origin.clone()])).await?;
    assert_eq!(outcome.text, "ok");
    assert_eq!(outcome.sources[0].title.as_deref(), Some("Example"));
    Ok(())
}

#[tokio::test]
async fn a_connect_failure_is_retried_on_a_fresh_authorization() -> TestResult {
    let closed = {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), port)?
    };
    let peer = peer(vec![sse_response(&answered())]).await?;
    let transport = fixed(vec![closed, peer.origin.clone()]);
    let outcome = run(&transport).await?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn a_rate_limit_is_retried_and_a_client_error_is_not() -> TestResult {
    let peer = peer(vec![
        http_response(429, "retry-after: 0\r\n", "{}"),
        sse_response(&answered()),
        http_response(
            400,
            "content-type: application/json\r\n",
            r#"{"error":{"code":400,"message":"web plugin is not available for this model"}}"#,
        ),
    ])
    .await?;
    let transport = fixed(vec![peer.origin.clone()]);
    assert_eq!(run(&transport).await?.text, "Rust 1.99 shipped.");
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 2);
    let error = run(&transport).await.err().ok_or("400 succeeded")?;
    assert_eq!(error.kind(), ChatRetrievalErrorKind::Rejected);
    assert_eq!(error.status(), Some(400));
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(
        error.message(),
        "the provider refused the side request (HTTP 400, web plugin is not available for this model)"
    );
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 3);
    Ok(())
}
