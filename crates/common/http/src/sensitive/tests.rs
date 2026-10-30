use std::error::Error;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::HeaderMap;

use super::{DropProbe, SensitiveBytes, sensitive_header_value};
use crate::{
    CancellationToken, Client, ErrorKind, ExchangeControl, LimitKind, Limits, Method, Origin,
    Request, RequestError, TransmissionFact,
};

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn bytes_clones_retain_one_owner_until_the_last_drop() {
    let probe = Arc::new(DropProbe::default());
    let sensitive = SensitiveBytes::observed(b"shared-secret".to_vec(), Arc::clone(&probe));
    let sensitive_clone = sensitive.clone();
    let bytes = sensitive.into_bytes();
    let bytes_clone = bytes.clone();
    let slice = bytes.slice(1..);

    drop(sensitive_clone);
    drop(bytes);
    drop(bytes_clone);
    assert_eq!(probe.drops(), 0);

    drop(slice);
    assert_eq!(probe.drops(), 1);
    assert!(probe.zeroized_before_drop());
}

#[test]
fn header_clones_retain_zeroizing_owner_and_debug_is_redacted() -> TestResult {
    let probe = Arc::new(DropProbe::default());
    let sensitive = SensitiveBytes::observed(b"Bearer shared-secret".to_vec(), Arc::clone(&probe));
    let value = sensitive_header_value(sensitive)?;
    let value_clone = value.clone();

    assert!(value.is_sensitive());
    assert!(!format!("{value:?}").contains("shared-secret"));
    drop(value);
    assert_eq!(probe.drops(), 0);

    drop(value_clone);
    assert_eq!(probe.drops(), 1);
    assert!(probe.zeroized_before_drop());
    Ok(())
}

#[test]
fn invalid_header_drops_and_zeroizes_owner() {
    let probe = Arc::new(DropProbe::default());
    let sensitive = SensitiveBytes::observed(b"secret\nvalue".to_vec(), Arc::clone(&probe));

    assert!(sensitive_header_value(sensitive).is_err());
    assert_eq!(probe.drops(), 1);
    assert!(probe.zeroized_before_drop());
}

#[test]
fn request_bounds_match_ordinary_bytes_and_debug_stays_content_free() -> TestResult {
    let origin = Origin::parse("https://example.test")?;
    let mut sensitive_headers = HeaderMap::new();
    sensitive_headers.insert(
        "x-secret",
        sensitive_header_value(b"header-secret".to_vec())?,
    );
    let mut ordinary_headers = HeaderMap::new();
    ordinary_headers.insert("x-secret", http::HeaderValue::from_static("header-secret"));
    let limits = Limits::default();
    let sensitive = Request::new(
        origin.clone(),
        Method::POST,
        "/request-secret",
        sensitive_headers,
        SensitiveBytes::new(b"body-secret".to_vec()).into_bytes(),
        &limits,
    )?;
    let ordinary = Request::new(
        origin.clone(),
        Method::POST,
        "/request-secret",
        ordinary_headers,
        bytes::Bytes::from_static(b"body-secret"),
        &limits,
    )?;

    assert_eq!(sensitive.body().len(), ordinary.body().len());
    assert_eq!(sensitive.headers().len(), ordinary.headers().len());
    let request_debug = format!("{sensitive:?}");
    let headers_debug = format!("{:?}", sensitive.headers());
    assert!(!request_debug.contains("body-secret"));
    assert!(!request_debug.contains("request-secret"));
    assert!(!headers_debug.contains("header-secret"));

    let body_limited = Limits {
        max_request_body_bytes: sensitive.body().len() - 1,
        ..Limits::default()
    };
    let sensitive_error = Request::new(
        origin.clone(),
        Method::POST,
        "/",
        HeaderMap::new(),
        SensitiveBytes::new(b"body-secret".to_vec()).into_bytes(),
        &body_limited,
    )
    .err()
    .ok_or_else(|| io::Error::other("sensitive request ignored body bound"))?;
    let ordinary_error = Request::new(
        origin.clone(),
        Method::POST,
        "/",
        HeaderMap::new(),
        bytes::Bytes::from_static(b"body-secret"),
        &body_limited,
    )
    .err()
    .ok_or_else(|| io::Error::other("ordinary request ignored body bound"))?;
    assert_eq!(
        sensitive_error,
        RequestError::LimitExceeded(LimitKind::RequestBodyBytes)
    );
    assert_eq!(sensitive_error, ordinary_error);

    let client = Client::new(limits)?;
    assert!(!format!("{client:?}").contains("body-secret"));
    assert!(!format!("{client:?}").contains("header-secret"));
    Ok(())
}

#[test]
fn sensitive_header_bounds_match_ordinary_headers() -> TestResult {
    let origin = Origin::parse("https://example.test")?;
    let mut sensitive_headers = HeaderMap::new();
    sensitive_headers.insert(
        "x-secret",
        sensitive_header_value(b"header-secret".to_vec())?,
    );
    let mut ordinary_headers = HeaderMap::new();
    ordinary_headers.insert("x-secret", http::HeaderValue::from_static("header-secret"));
    let limits = Limits {
        max_request_header_bytes: 0,
        ..Limits::default()
    };
    let sensitive_error = Request::new(
        origin.clone(),
        Method::POST,
        "/",
        sensitive_headers,
        bytes::Bytes::new(),
        &limits,
    )
    .err()
    .ok_or_else(|| io::Error::other("sensitive request ignored header bound"))?;
    let ordinary_error = Request::new(
        origin,
        Method::POST,
        "/",
        ordinary_headers,
        bytes::Bytes::new(),
        &limits,
    )
    .err()
    .ok_or_else(|| io::Error::other("ordinary request ignored header bound"))?;

    assert_eq!(
        sensitive_error,
        RequestError::LimitExceeded(LimitKind::RequestHeaderBytes)
    );
    assert_eq!(sensitive_error, ordinary_error);
    Ok(())
}

#[tokio::test]
async fn cancellation_drops_request_owner() -> TestResult {
    let probe = Arc::new(DropProbe::default());
    let body = SensitiveBytes::observed(b"cancel-secret".to_vec(), Arc::clone(&probe));
    let limits = Limits::default();
    let request = Request::new(
        Origin::parse("https://localhost")?,
        Method::POST,
        "/",
        HeaderMap::new(),
        body.into_bytes(),
        &limits,
    )?;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let failure = Client::new(limits)?
        .execute(
            request,
            ExchangeControl::with_cancellation(
                Instant::now() + Duration::from_secs(1),
                cancellation,
            ),
        )
        .await
        .err()
        .ok_or_else(|| io::Error::other("pre-cancelled exchange unexpectedly succeeded"))?;

    assert_eq!(failure.kind(), ErrorKind::Cancelled);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert_eq!(probe.drops(), 1);
    assert!(probe.zeroized_before_drop());
    Ok(())
}

#[tokio::test]
async fn validation_error_drops_request_owner() -> TestResult {
    let probe = Arc::new(DropProbe::default());
    let body = SensitiveBytes::observed(b"error-secret".to_vec(), Arc::clone(&probe));
    let construction_limits = Limits::default();
    let request = Request::new(
        Origin::parse("https://localhost")?,
        Method::POST,
        "/",
        HeaderMap::new(),
        body.into_bytes(),
        &construction_limits,
    )?;
    let execution_limits = Limits {
        max_request_body_bytes: 1,
        ..Limits::default()
    };

    let failure = Client::new(execution_limits)?
        .execute(
            request,
            ExchangeControl::new(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .err()
        .ok_or_else(|| io::Error::other("client revalidation unexpectedly succeeded"))?;

    assert_eq!(
        failure.kind(),
        ErrorKind::LimitExceeded(LimitKind::RequestBodyBytes)
    );
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert_eq!(probe.drops(), 1);
    assert!(probe.zeroized_before_drop());
    Ok(())
}
