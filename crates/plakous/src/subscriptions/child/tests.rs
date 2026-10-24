#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "offline lookup fixtures"
)]

use super::*;
use crate::subscriptions::{
    NativeProvider, NativeSubscriptions, SubscriptionPaths, map_codex_error,
};
use plakous_exchange::lookup::{LookupBudget, get_json};
use plakous_http::{Bytes, Client, HeaderMap, Limits, Method, Origin, Request};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum Reply {
    StalledBody,
    Status(u16),
}

async fn server(replies: Vec<Reply>) -> (Origin, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = Origin::loopback_http(
        listener.local_addr().unwrap().ip(),
        listener.local_addr().unwrap().port(),
    )
    .unwrap();
    let gets = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&gets);
    let task = tokio::spawn(async move {
        let mut workers = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await.unwrap());
            }
            assert!(
                head.starts_with(
                    b"GET /backend-api/codex/models?client_version=0.162.1 HTTP/1.1\r\n"
                )
            );
            counted.fetch_add(1, Ordering::SeqCst);
            workers.push(tokio::spawn(async move {
                match reply {
                    Reply::StalledBody => {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 65536\r\n\r\n").await.unwrap();
                        stream.write_all(&vec![b' '; 32768]).await.unwrap();
                        // Stay open until the client's body deadline closes
                        // its transport; never manufacture an early EOF.
                        let mut byte = [0];
                        let _ = stream.read(&mut byte).await;
                    }
                    Reply::Status(status) => {
                        stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();
                    }
                }
            }));
        }
        for worker in workers {
            worker.await.unwrap();
        }
    });
    (origin, gets, task)
}

async fn lookup(
    client: &Client,
    origin: &Origin,
    model: &ModelId,
) -> Result<(), NativeModelResolutionError> {
    let request = Request::new(
        origin.clone(),
        Method::GET,
        "/backend-api/codex/models?client_version=0.162.1",
        HeaderMap::new(),
        Bytes::new(),
        client.limits(),
    )
    .unwrap();
    let reply = get_json(
        client,
        request,
        LookupBudget::single_attempt(std::time::Instant::now() + Duration::from_secs(2)),
        65536,
    )
    .await
    .map_err(|error| {
        NativeModelResolutionError::after_selection(
            model.clone(),
            map_codex_error(plakous_openai::NativeCodexAuthError::Lookup(error)),
        )
    })?;
    if matches!(reply.status.as_u16(), 401 | 403) {
        return Err(NativeModelResolutionError::after_selection(
            model.clone(),
            NativeSubscriptionError::AuthenticationProviderRejected,
        ));
    }
    assert_eq!(reply.status.as_u16(), 200);
    Ok(())
}

#[tokio::test]
async fn stalled_body_retries_only_after_operation_locks_and_lease_are_released() {
    let (origin, gets, task) = server(vec![Reply::StalledBody, Reply::Status(200)]).await;
    let directory = tempfile::tempdir().unwrap();
    let subscriptions =
        NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(directory.path()).unwrap());
    let lifecycle = tokio::sync::Mutex::new(());
    let ready = tokio::sync::Semaphore::new(1);
    let waits = AtomicUsize::new(0);
    let model = ModelId::new("gpt-5.4").unwrap();
    let client = Client::new_http1(Limits {
        response_body_timeout: Duration::from_millis(50),
        ..Limits::default()
    })
    .unwrap();
    resolve(
        |_| async {
            let _compatibility = subscriptions
                .acquire_compatibility_lock(NativeProvider::Codex)
                .await
                .unwrap();
            let _lifecycle = lifecycle.lock().await;
            let _ready = ready.acquire().await.unwrap();
            lookup(&client, &origin, &model).await
        },
        &model,
        None,
        |delay| {
            let subscriptions = &subscriptions;
            let lifecycle = &lifecycle;
            let ready = &ready;
            let waits = &waits;
            async move {
                assert!((Duration::from_millis(500)..=Duration::from_millis(750)).contains(&delay));
                assert!(
                    subscriptions
                        .try_acquire_compatibility_lock(NativeProvider::Codex)
                        .await
                        .unwrap()
                        .is_some()
                );
                assert!(lifecycle.try_lock().is_ok());
                assert!(ready.try_acquire().is_ok());
                waits.fetch_add(1, Ordering::SeqCst);
            }
        },
    )
    .await
    .unwrap();
    task.await.unwrap();
    assert_eq!(gets.load(Ordering::SeqCst), 2);
    assert_eq!(waits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistent_body_stall_retains_total_attempts_and_cause() {
    let (origin, gets, task) = server(vec![Reply::StalledBody, Reply::StalledBody]).await;
    let model = ModelId::new("gpt-5.4").unwrap();
    let client = Client::new_http1(Limits {
        response_body_timeout: Duration::from_millis(50),
        ..Limits::default()
    })
    .unwrap();
    let error = resolve(
        |_| lookup(&client, &origin, &model),
        &model,
        None,
        |_| async {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "model lookup failed after 2 attempts: transport timeout"
    );
    task.await.unwrap();
    assert_eq!(gets.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authentication_policy_and_model_absence_do_not_retry() {
    let model = ModelId::new("gpt-5.4").unwrap();
    let client = Client::new_http1(Limits::default()).unwrap();
    for status in [401, 403] {
        let (origin, gets, task) = server(vec![Reply::Status(status)]).await;
        let error = resolve(
            |_| lookup(&client, &origin, &model),
            &model,
            None,
            |_| async { panic!("authentication failure retried") },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.kind(),
            NativeSubscriptionError::AuthenticationProviderRejected
        );
        task.await.unwrap();
        assert_eq!(gets.load(Ordering::SeqCst), 1);
    }
    for kind in [
        NativeSubscriptionError::ModelUnavailable,
        NativeSubscriptionError::CatalogDocumentRejected,
        NativeSubscriptionError::ProviderConfigurationPolicyMismatch("processing_tier"),
    ] {
        let attempts = AtomicUsize::new(0);
        let _ = resolve(
            |_| {
                attempts.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err::<(), _>(NativeModelResolutionError::after_selection(
                    model.clone(),
                    kind,
                )))
            },
            &model,
            None,
            |_| async { panic!("permanent error retried") },
        )
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn provider_retry_after_is_a_minimum_and_cannot_exceed_the_shared_deadline() {
    let model = ModelId::new("gpt-5.4").unwrap();
    for (seconds, remaining, retry) in [(10, 120, true), (16, 120, false), (10, 45, false)] {
        let calls = AtomicUsize::new(0);
        let waits = AtomicUsize::new(0);
        let caller_deadline = tokio::time::Instant::now() + Duration::from_secs(remaining);
        let shared_deadline = caller_deadline - STARTUP_MARGIN;
        let result = resolve(
            |deadline| {
                assert_eq!(deadline, shared_deadline);
                let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                std::future::ready(if first {
                    Err(NativeModelResolutionError::after_selection(
                        model.clone(),
                        NativeSubscriptionError::ModelLookup {
                            attempts: 1,
                            cause: "HTTP 429",
                            retry_after: Some(Duration::from_secs(seconds)),
                        },
                    ))
                } else {
                    Ok(())
                })
            },
            &model,
            Some(caller_deadline),
            |delay| {
                waits.fetch_add(1, Ordering::SeqCst);
                assert_eq!(delay, Duration::from_secs(seconds));
                tokio::time::sleep(delay)
            },
        )
        .await;
        assert_eq!(result.is_ok(), retry);
        assert_eq!(calls.load(Ordering::SeqCst), if retry { 2 } else { 1 });
        assert_eq!(waits.load(Ordering::SeqCst), usize::from(retry));
        if !retry {
            assert_eq!(
                result.unwrap_err().to_string(),
                "model lookup failed after 1 attempt: HTTP 429"
            );
        }
    }
}

#[tokio::test]
async fn successful_resolution_uses_one_get_without_retry_or_backoff() {
    let (origin, gets, task) = server(vec![Reply::Status(200)]).await;
    let client = Client::new_http1(Limits::default()).unwrap();
    let model = ModelId::new("gpt-5.4").unwrap();
    resolve(
        |_| lookup(&client, &origin, &model),
        &model,
        None,
        |_| async { panic!("success retried") },
    )
    .await
    .unwrap();
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    task.await.unwrap();
}
