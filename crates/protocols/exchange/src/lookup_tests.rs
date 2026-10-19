#![allow(clippy::unwrap_used, reason = "test fixtures and assertions")]

use super::lookup::*;
use plakous_http::{ErrorKind, Phase};
use std::time::Duration;

async fn run<F, Fut>(
    timeout: Duration,
    cap: Duration,
    exchange: F,
    notify: impl FnMut(u8, &str),
) -> Result<LookupReply, LookupError>
where
    F: FnMut(std::time::Instant) -> Fut,
    Fut: Future<Output = Result<LookupReply, LookupError>>,
{
    super::lookup::run(timeout, cap, exchange, notify, &mut |_| Ok(()), 3).await
}

#[test]
fn lookup_status_retry_classes() {
    for status in [429, 500, 502, 503, 504] {
        assert!(LookupCause::Status(status).retryable());
    }
    for status in [200, 400, 401, 403, 404, 408, 409, 422, 501, 505] {
        assert!(!LookupCause::Status(status).retryable());
    }
}

#[test]
fn lookup_transport_retry_classes_ignore_transmission() {
    for (phase, kind) in [
        (Phase::Resolution, ErrorKind::Resolution),
        (Phase::Connect, ErrorKind::Io),
        (Phase::ResponseHead, ErrorKind::DeadlineExceeded),
        (Phase::ResponseBody, ErrorKind::Io),
        (Phase::ResponseBody, ErrorKind::Protocol),
    ] {
        assert!(retryable_transport(phase, kind));
    }
    for kind in [
        ErrorKind::Cancelled,
        ErrorKind::Internal,
        ErrorKind::AddressRefused,
        ErrorKind::Tls,
    ] {
        assert!(!retryable_transport(Phase::Connect, kind));
    }
}

#[test]
fn lookup_backoff_and_cap() {
    for attempt in [1, 2] {
        let base = if attempt == 1 { 500 } else { 2000 };
        let delay = retry_delay(attempt, None, 0);
        assert!(delay >= Duration::from_millis(base));
        assert!(retry_delay(attempt, None, 255) <= Duration::from_millis(base + 250));
    }
    assert_eq!(
        retry_delay(1, Some(Duration::from_secs(10)), 0),
        Duration::from_secs(10)
    );
    assert!(!can_retry(3, Duration::ZERO, Duration::from_secs(60)));
    assert!(!can_retry(
        1,
        Duration::from_secs(60),
        Duration::from_secs(60)
    ));
    assert!(can_retry(2, Duration::from_secs(2), Duration::from_secs(3)));
}

fn reply(status: u16, bytes: &[u8]) -> LookupReply {
    reply_with_advice(status, bytes, None)
}

fn reply_with_advice(status: u16, bytes: &[u8], advice: Option<&str>) -> LookupReply {
    let mut headers = plakous_http::HeaderMap::new();
    if let Some(advice) = advice {
        headers.insert(
            plakous_http::HeaderName::from_static("retry-after"),
            plakous_http::HeaderValue::from_str(advice).unwrap(),
        );
    }
    let request = plakous_http::Request::new(
        plakous_http::Origin::loopback_http(std::net::Ipv4Addr::LOCALHOST.into(), 1234).unwrap(),
        plakous_http::Method::GET,
        "/models",
        headers,
        plakous_http::Bytes::new(),
        &plakous_http::Limits::default(),
    )
    .unwrap();
    LookupReply {
        status: plakous_http::StatusCode::from_u16(status).unwrap(),
        headers: request.headers().clone(),
        body: zeroize::Zeroizing::new(bytes.to_vec()),
    }
}

#[tokio::test(start_paused = true)]
async fn lookup_retry_returns_only_successful_snapshot() {
    let mut attempts = 0;
    let mut retries = Vec::new();
    let result = run(
        Duration::from_secs(20),
        Duration::from_secs(60),
        |_| {
            attempts += 1;
            let value = reply(if attempts == 1 { 503 } else { 200 }, &[attempts]);
            async { Ok(value) }
        },
        |attempt, _| retries.push(attempt),
    )
    .await
    .unwrap();
    assert_eq!(attempts, 2);
    assert_eq!(retries, [2]);
    assert_eq!(*result.body, [2]);
}

#[tokio::test(start_paused = true)]
async fn lookup_retry_exhaustion_counts_real_attempts() {
    let mut attempts = 0;
    let mut retries = Vec::new();
    let result = run(
        Duration::from_secs(20),
        Duration::from_secs(60),
        |_| {
            attempts += 1;
            async { Ok(reply(503, b"")) }
        },
        |attempt, _| retries.push(attempt),
    )
    .await;
    assert!(matches!(
        result,
        Err(LookupError::Exhausted {
            attempts: 3,
            cause: LookupCause::Status(503)
        })
    ));
    assert_eq!(attempts, 3);
    assert_eq!(retries, [2, 3]);
}

#[tokio::test(start_paused = true)]
async fn lookup_retry_cap_prevents_sleep_or_redispatch() {
    let mut attempts = 0;
    let result = run(
        Duration::from_secs(20),
        Duration::from_millis(400),
        |_| {
            attempts += 1;
            async { Ok(reply(429, b"")) }
        },
        |_, _| panic!("must not announce an impossible retry"),
    )
    .await;
    assert!(matches!(
        result,
        Err(LookupError::Exhausted { attempts: 1, .. })
    ));
    assert_eq!(attempts, 1);
}

#[tokio::test(start_paused = true)]
async fn lookup_never_retries_validation_or_permanent_answers() {
    for status in [200, 401, 403, 404, 422] {
        let mut attempts = 0;
        let result = run(
            Duration::from_secs(20),
            Duration::from_secs(60),
            |_| {
                attempts += 1;
                async move { Ok(reply(status, b"not JSON or valid evidence")) }
            },
            |_, _| panic!("permanent answer retried"),
        )
        .await
        .unwrap();
        assert_eq!(result.status.as_u16(), status);
        assert_eq!(attempts, 1);
    }
    for error in [
        LookupError::BodyLimit,
        LookupError::MediaType,
        LookupError::NotGet,
    ] {
        let mut attempts = 0;
        let result = run(
            Duration::from_secs(20),
            Duration::from_secs(60),
            |_| {
                attempts += 1;
                async move { Err(error) }
            },
            |_, _| panic!("local validation failure retried"),
        )
        .await;
        assert!(matches!(result, Err(actual) if actual == error));
        assert_eq!(attempts, 1);
    }
}

#[tokio::test(start_paused = true)]
async fn lookup_retry_after_honored_without_crossing_cap() {
    let started = tokio::time::Instant::now();
    let mut attempts = 0;
    let result = run(
        Duration::from_secs(20),
        Duration::from_secs(60),
        |_| {
            attempts += 1;
            let value = if attempts == 1 {
                reply_with_advice(429, b"", Some("5"))
            } else {
                reply(200, b"ok")
            };
            async { Ok(value) }
        },
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status.as_u16(), 200);
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(5)
    );

    let mut attempts = 0;
    let result = run(
        Duration::from_secs(20),
        Duration::from_secs(60),
        |_| {
            attempts += 1;
            async { Ok(reply_with_advice(429, b"", Some("61"))) }
        },
        |_, _| panic!("Retry-After exceeds cap"),
    )
    .await;
    assert!(matches!(
        result,
        Err(LookupError::Exhausted { attempts: 1, .. })
    ));
    assert_eq!(attempts, 1);
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(5)
    );
}

#[tokio::test(start_paused = true)]
async fn lookup_attempt_deadlines_are_renewed_and_clamped() {
    let started = tokio::time::Instant::now();
    let mut attempts = 0;
    let result = run(
        Duration::from_secs(30),
        Duration::from_secs(60),
        |deadline| {
            attempts += 1;
            let now = tokio::time::Instant::now();
            let expected = (now + Duration::from_secs(30)).min(started + Duration::from_secs(60));
            assert_eq!(deadline, expected.into_std());
            async move {
                tokio::time::sleep_until(expected).await;
                Ok(reply(503, b""))
            }
        },
        |_, _| {},
    )
    .await;
    assert!(matches!(
        result,
        Err(LookupError::Exhausted { attempts: 2, .. })
    ));
    assert_eq!(attempts, 2);
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(60)
    );
}

#[tokio::test(start_paused = true)]
async fn lookup_retry_after_leaves_useful_attempt_time() {
    let started = tokio::time::Instant::now();
    let mut attempts = 0;
    let result = run(
        Duration::from_secs(20),
        Duration::from_secs(60),
        |_| {
            attempts += 1;
            let value = if attempts == 1 {
                reply_with_advice(429, b"", Some("59"))
            } else {
                reply(200, b"ok")
            };
            async { Ok(value) }
        },
        |_, _| {},
    )
    .await;
    assert!(matches!(
        result,
        Err(LookupError::Exhausted {
            attempts: 1,
            cause: LookupCause::Status(429)
        })
    ));
    assert_eq!(tokio::time::Instant::now(), started);
}

#[tokio::test(start_paused = true)]
async fn lookup_zero_budget_keeps_plain_transport_error() {
    let client = plakous_http::Client::new(plakous_http::Limits::default()).unwrap();
    let request = plakous_http::Request::new(
        plakous_http::Origin::loopback_http(std::net::Ipv4Addr::LOCALHOST.into(), 1234).unwrap(),
        plakous_http::Method::GET,
        "/models",
        plakous_http::HeaderMap::new(),
        plakous_http::Bytes::new(),
        client.limits(),
    )
    .unwrap();
    let failure = client
        .execute(
            request,
            plakous_http::ExchangeControl::new(std::time::Instant::now()),
        )
        .await
        .unwrap_err();
    for cap in [Duration::ZERO, Duration::from_secs(60)] {
        let result = run(
            Duration::ZERO,
            cap,
            |deadline| {
                assert!(
                    deadline <= tokio::time::Instant::now().into_std(),
                    "an expired attempt borrowed the operation cap"
                );
                async { Err(LookupError::Transport(failure)) }
            },
            |_, _| panic!("zero budget retried"),
        )
        .await;
        assert!(matches!(result, Err(LookupError::Transport(_))));
    }
}

#[tokio::test(start_paused = true)]
async fn lookup_single_attempt_renewal_does_not_retry_transient_status() {
    let mut attempts = 0;
    let value = super::lookup::run(
        Duration::from_secs(30),
        Duration::from_secs(60),
        |_| {
            attempts += 1;
            async { Ok(reply(503, b"")) }
        },
        |_, _| panic!("renewal retried"),
        &mut |_| Ok(()),
        1,
    )
    .await
    .unwrap();
    assert_eq!(value.status.as_u16(), 503);
    assert_eq!(attempts, 1);
}
