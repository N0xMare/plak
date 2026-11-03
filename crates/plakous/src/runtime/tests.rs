use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{Runtime, TokioRuntime};

#[tokio::test(start_paused = true)]
async fn tokio_runtime_spawns_sleeps_and_reads_monotonic_time() {
    let runtime = TokioRuntime::new();
    let before = runtime.now();
    let ran = Arc::new(AtomicBool::new(false));
    let task = runtime.spawn(Box::pin({
        let ran = Arc::clone(&ran);
        let sleep = runtime.sleep(Duration::from_secs(5));
        async move {
            sleep.await;
            ran.store(true, Ordering::Release);
        }
    }));
    assert!(!task.is_finished());
    runtime.sleep(Duration::from_secs(6)).await;
    assert!(ran.load(Ordering::Acquire));
    assert!(task.is_finished());
    assert!(runtime.now() >= before + Duration::from_secs(6));
}

#[tokio::test]
async fn an_aborted_task_is_dropped_before_it_finishes() {
    let runtime = TokioRuntime::new();
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    let task = runtime.spawn(Box::pin(async move {
        std::future::pending::<()>().await;
        drop(sender);
    }));
    task.abort();
    assert!(
        receiver.await.is_err(),
        "the aborted task dropped its future"
    );
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
}

#[cfg(any(
    feature = "anthropic",
    feature = "claude",
    feature = "codex",
    feature = "grok",
    feature = "openai",
    feature = "openrouter",
    feature = "web-fetch",
    feature = "xai"
))]
#[tokio::test]
async fn native_http_transport_refuses_a_request_it_cannot_frame_before_sending() {
    use super::http::NativeHttpTransport;
    use super::{HttpRequest, HttpTransport, HttpTransportErrorKind};

    let client =
        plakous_http::Client::new(plakous_http::Limits::for_streaming_inference()).unwrap();
    let transport = NativeHttpTransport::new(client, Duration::from_secs(1));
    for request in [
        HttpRequest::new("GET", "http://example.invalid/"),
        HttpRequest::new("GET", "https://user@example.invalid/"),
        HttpRequest::new("NOT A METHOD", "https://example.invalid/"),
        HttpRequest::new("GET", "https://example.invalid/").header("bad name", "x"),
    ] {
        let error = transport.send(request).await.unwrap_err();
        assert_eq!(error.kind(), HttpTransportErrorKind::InvalidRequest);
    }
}
