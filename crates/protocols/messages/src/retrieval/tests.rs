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

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn search() -> MessagesRetrievalRequest {
    MessagesRetrievalRequest::new(
        "claude-sonnet-4-6",
        MessagesRetrievalQuery::WebSearch {
            query: "rust 1.99".to_owned(),
            allowed_domains: vec!["rust-lang.org".to_owned()],
            blocked_domains: Vec::new(),
        },
    )
}

fn fetch() -> MessagesRetrievalRequest {
    MessagesRetrievalRequest::new(
        "claude-sonnet-4-6",
        MessagesRetrievalQuery::PageFetch {
            url: "https://example.com/a".to_owned(),
            prompt: Some("list the headings".to_owned()),
        },
    )
}

#[test]
fn web_search_body_is_exact_and_cost_only() -> TestResult {
    let body = search().body(&[])?;
    assert_eq!(
        String::from_utf8(body.clone())?,
        concat!(
            r#"{"model":"claude-sonnet-4-6","max_tokens":4096,"messages":[{"content":[{"text":"#,
            r#""Search the web for: rust 1.99. Answer concisely and cite sources.","type":"text"}],"#,
            r#""role":"user"}],"tools":[{"allowed_callers":["direct"],"allowed_domains":["rust-lang.org"],"#,
            r#""max_uses":5,"name":"web_search","type":"web_search_20260318"}],"stream":true}"#,
        )
    );
    assert_eq!(
        classify_messages_retrieval_body(&body),
        ReplaySafety::CostOnly
    );
    Ok(())
}

#[test]
fn web_fetch_body_is_exact_and_cost_only() -> TestResult {
    let body = fetch()
        .with_system(vec!["costume line".to_owned()])
        .with_metadata(json!({"user_id": "u"}))
        .body(&[])?;
    assert_eq!(
        String::from_utf8(body.clone())?,
        concat!(
            r#"{"model":"claude-sonnet-4-6","max_tokens":4096,"system":[{"type":"text","text":"costume line"}],"#,
            r#""messages":[{"content":[{"text":"Fetch https://example.com/a and answer: list the headings. "#,
            r#"Answer concisely and cite sources.","type":"text"}],"role":"user"}],"tools":[{"allowed_callers":"#,
            r#"["direct"],"citations":{"enabled":true},"max_content_tokens":20000,"max_uses":1,"#,
            r#""name":"web_fetch","type":"web_fetch_20260318"}],"metadata":{"user_id":"u"},"stream":true}"#,
        )
    );
    assert_eq!(
        classify_messages_retrieval_body(&body),
        ReplaySafety::CostOnly
    );
    Ok(())
}

#[test]
fn continuation_body_sends_paused_content_back_unchanged() -> TestResult {
    let paused = vec![
        json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "q"}, "encrypted_content": "opaque=="}),
    ];
    let body: Value = serde_json::from_slice(&search().body(&paused)?)?;
    assert_eq!(
        body["messages"][1],
        json!({"role": "assistant", "content": paused})
    );
    Ok(())
}

#[test]
fn only_allowlisted_one_tool_bodies_are_cost_only() -> TestResult {
    let base: Value = serde_json::from_slice(&search().body(&[])?)?;
    let mutate = |change: &dyn Fn(&mut Value)| {
        let mut body = base.clone();
        change(&mut body);
        classify_messages_retrieval_body(&serde_json::to_vec(&body).unwrap_or_default())
    };
    let server_effect = [
        mutate(&|body| body["tools"][0]["type"] = json!("code_execution_20260120")),
        mutate(&|body| body["tools"][0]["type"] = json!("web_search_20990101")),
        mutate(&|body| body["tools"][0]["allowed_callers"] = json!(["code_execution_20260120"])),
        mutate(&|body| {
            if let Some(tool) = body["tools"][0].as_object_mut() {
                tool.remove("allowed_callers");
            }
        }),
        mutate(&|body| body["tools"][0]["defer_loading"] = json!(true)),
        mutate(&|body| {
            let tool = body["tools"][0].clone();
            body["tools"] = json!([tool.clone(), tool]);
        }),
        mutate(&|body| body["container"] = json!("c")),
        classify_messages_retrieval_body(b"not json"),
    ];
    for safety in server_effect {
        assert_eq!(safety, ReplaySafety::ServerEffect);
    }
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

fn message_start() -> Value {
    json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 100, "output_tokens": 1}}})
}

fn stop(reason: &str, searches: u64) -> [Value; 2] {
    [
        json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 40, "server_tool_use": {"web_search_requests": searches}}}),
        json!({"type": "message_stop"}),
    ]
}

fn block(index: usize, block: Value) -> [Value; 2] {
    [
        json!({"type": "content_block_start", "index": index, "content_block": block}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn text_block(index: usize, text: &str, citations: Value) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "citations_delta", "citation": citations}}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn search_use(index: usize) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": index, "content_block": {"type": "server_tool_use", "id": format!("srvtoolu_{index}"), "name": "web_search", "input": {}}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":"}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": "\"rust\"}"}}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn search_result(index: usize, content: Value) -> [Value; 2] {
    block(
        index,
        json!({"type": "web_search_tool_result", "tool_use_id": format!("srvtoolu_{}", index - 1), "content": content}),
    )
}

fn answered_search() -> String {
    let mut events = vec![message_start()];
    events.extend(text_block(0, "I'll search.", json!({})));
    events.extend(search_use(1));
    events.extend(search_result(
        2,
        json!([
            {"type": "web_search_result", "url": "https://blog.rust-lang.org/1.99", "title": "Announcing Rust 1.99", "encrypted_content": "e1"},
            {"type": "web_search_result", "url": "https://example.com/", "title": "", "encrypted_content": "e2"}
        ]),
    ));
    events.extend(text_block(
        3,
        "Rust 1.99 shipped.",
        json!({"type": "web_search_result_location", "url": "https://blog.rust-lang.org/1.99", "title": "Announcing Rust 1.99", "cited_text": "x", "encrypted_index": "i"}),
    ));
    events.extend(stop("end_turn", 1));
    sse(&events)
}

fn collect(body: &str) -> Result<Finished, MessagesRetrievalError> {
    let mut collector = Collector::new(false);
    for chunk in body.as_bytes().chunks(11) {
        collector.push(chunk).map_err(|broken| match broken {
            Broken::Overloaded(error) | Broken::Fatal(error) => error,
        })?;
    }
    collector.finish()
}

#[test]
fn search_stream_yields_the_answer_after_the_last_result_and_unique_sources() -> TestResult {
    let finished = collect(&answered_search())?;
    assert_eq!(finished.stop, "end_turn");
    assert_eq!(finished.content[1]["input"], json!({"query": "rust"}));
    let outcome = assemble(&finished.content, finished.usage, 1)?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(
        outcome.sources,
        [
            MessagesRetrievalSource {
                url: "https://blog.rust-lang.org/1.99".to_owned(),
                title: Some("Announcing Rust 1.99".to_owned()),
            },
            MessagesRetrievalSource {
                url: "https://example.com/".to_owned(),
                title: None,
            },
        ]
    );
    assert_eq!(
        outcome.usage,
        MessagesRetrievalUsage {
            input_tokens: 100,
            output_tokens: 40,
            hosted_requests: 1,
        }
    );
    Ok(())
}

#[test]
fn max_uses_exceeded_without_an_answer_is_an_error() -> TestResult {
    let mut events = vec![message_start()];
    events.extend(search_use(0));
    events.extend(search_result(
        1,
        json!({"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}),
    ));
    events.extend(stop("end_turn", 5));
    let finished = collect(&sse(&events))?;
    let error = assemble(&finished.content, finished.usage, 1).err();
    assert_eq!(
        error.map(|error| (error.kind(), error.message().to_owned())),
        Some((
            MessagesRetrievalErrorKind::Rejected,
            "the provider's retrieval failed (max_uses_exceeded)".to_owned()
        ))
    );
    Ok(())
}

#[test]
fn an_answer_without_sources_is_kept() -> TestResult {
    let mut events = vec![message_start()];
    events.extend(search_use(0));
    events.extend(search_result(1, json!([])));
    events.extend(text_block(2, "Nothing found.", json!({})));
    events.extend(stop("end_turn", 1));
    let finished = collect(&sse(&events))?;
    let outcome = assemble(&finished.content, finished.usage, 1)?;
    assert_eq!(outcome.text, "Nothing found.");
    assert_eq!(outcome.sources, []);
    Ok(())
}

#[test]
fn fetch_result_reports_its_page_as_a_source() -> TestResult {
    let mut events = vec![message_start()];
    events.extend(block(
        0,
        json!({"type": "server_tool_use", "id": "srvtoolu_f", "name": "web_fetch", "input": {"url": "https://example.com/a"}}),
    ));
    events.extend(block(
        1,
        json!({"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f", "content": {"type": "web_fetch_result", "url": "https://example.com/a", "content": {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "page"}, "title": "Example A"}}}),
    ));
    events.extend(text_block(2, "Headings: A.", json!({})));
    events.extend(stop("end_turn", 0));
    let finished = collect(&sse(&events))?;
    let outcome = assemble(&finished.content, finished.usage, 1)?;
    assert_eq!(outcome.text, "Headings: A.");
    assert_eq!(
        outcome.sources,
        [MessagesRetrievalSource {
            url: "https://example.com/a".to_owned(),
            title: Some("Example A".to_owned()),
        }]
    );
    Ok(())
}

#[test]
fn the_side_grammar_refuses_what_its_body_did_not_offer() {
    let refused = [
        block(
            0,
            json!({"type": "server_tool_use", "id": "srvtoolu_c", "name": "code_execution", "input": {}}),
        )
        .to_vec(),
        block(0, json!({"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}})).to_vec(),
        block(0, json!({"type": "thinking", "thinking": "", "signature": ""})).to_vec(),
        block(
            0,
            json!({"type": "bash_code_execution_tool_result", "tool_use_id": "x", "content": {}}),
        )
        .to_vec(),
    ];
    for blocks in refused {
        let mut events = vec![message_start()];
        events.extend(blocks);
        events.extend(stop("end_turn", 0));
        let error = collect(&sse(&events)).err();
        assert_eq!(
            error.map(|error| error.kind()),
            Some(MessagesRetrievalErrorKind::InvalidResponse)
        );
    }
}

#[test]
fn a_stream_cut_before_message_stop_is_retryable_unavailability() {
    let body = answered_search();
    let cut = &body[..body.len() - 40];
    let error = collect(cut).err();
    assert_eq!(
        error.map(|error| error.kind()),
        Some(MessagesRetrievalErrorKind::Unavailable)
    );
}

// --- The runner over loopback HTTP ---------------------------------------

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

impl MessagesRetrievalTransport for Fixed {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> MessagesRetrievalAuthorization<'a> {
        Box::pin(async move {
            let index = self.authorized.fetch_add(1, Ordering::SeqCst);
            let origin = self.origins[index.min(self.origins.len() - 1)].clone();
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            HttpRequest::new(
                origin,
                Method::POST,
                "/v1/messages",
                headers,
                Bytes::copy_from_slice(body),
                &Limits::default(),
            )
            .map_err(|_| MessagesRetrievalError::invalid_request("fixture request"))
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
    request: &MessagesRetrievalRequest,
) -> Result<MessagesRetrievalOutcome, MessagesRetrievalError> {
    let client = Client::new_http1(Limits::default())
        .map_err(|_| MessagesRetrievalError::invalid_request("fixture client"))?;
    run_messages_retrieval(
        &client,
        transport,
        request,
        &CancellationToken::new(),
        MessagesRetrievalLimits {
            exchange_timeout: Duration::from_secs(10),
            jitter: Some(Jitter::SHORTEST),
        },
    )
    .await
}

fn paused() -> String {
    let mut events = vec![message_start()];
    events.extend(search_use(0));
    events.extend(stop("pause_turn", 1));
    sse(&events)
}

#[tokio::test]
async fn pause_turn_continues_inside_the_side_request() -> TestResult {
    let peer = peer(vec![
        sse_response(&paused()),
        sse_response(&answered_search()),
    ])
    .await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let outcome = run(&transport, &search()).await?;
    assert_eq!(outcome.exchanges, 2);
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(outcome.usage.input_tokens, 200);
    assert_eq!(outcome.usage.hosted_requests, 2);
    let requests = peer.requests.lock().map_err(|_| "poisoned")?.clone();
    assert_eq!(requests.len(), 2);
    let continued: Value = serde_json::from_slice(&requests[1])?;
    assert_eq!(continued["messages"][1]["role"], "assistant");
    assert_eq!(
        continued["messages"][1]["content"][0]["input"],
        json!({"query": "rust"})
    );
    Ok(())
}

#[tokio::test]
async fn a_fourth_pause_ends_the_side_request() -> TestResult {
    let peer = peer(vec![sse_response(&paused()); 4]).await?;
    let transport = fixed(vec![peer.origin.clone()]);
    let error = run(&transport, &search()).await.err();
    assert_eq!(
        error.map(|error| error.kind()),
        Some(MessagesRetrievalErrorKind::Unavailable)
    );
    assert_eq!(peer.requests.lock().map_err(|_| "poisoned")?.len(), 4);
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
    let peer = peer(vec![sse_response(&answered_search())]).await?;
    let transport = fixed(vec![closed, peer.origin.clone()]);
    let outcome = run(&transport, &search()).await?;
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
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"tool type not supported"}}"#,
        ),
    ])
    .await?;
    let transport = fixed(vec![peer.origin.clone()]);
    assert_eq!(run(&transport, &search()).await?.text, "Rust 1.99 shipped.");
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 2);
    let error = run(&transport, &search())
        .await
        .err()
        .ok_or("400 succeeded")?;
    assert_eq!(error.kind(), MessagesRetrievalErrorKind::Rejected);
    assert_eq!(error.status(), Some(400));
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(
        error.message(),
        "the provider refused the side request (HTTP 400, invalid_request_error: tool type not supported)"
    );
    assert_eq!(transport.authorized.load(Ordering::SeqCst), 3);
    Ok(())
}
