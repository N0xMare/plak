//! Bounded, secret-bearing bodies for authentication exchanges.

use plakous_http::{Failure, ResponseBody};
use thiserror::Error;
use zeroize::Zeroizing;

/// A content-free authentication body failure.
#[derive(Debug, Error)]
pub enum AuthBodyError {
    /// The transport failed while pulling the body.
    #[error("authentication body transport failed")]
    Failure(#[from] Failure),
    /// The body exceeded the caller's bound.
    #[error("authentication body too large")]
    TooLarge,
}

/// Pulls an ordinary authentication response through EOF into zeroizing storage.
/// Closes the body before returning a size refusal. Claimed refresh responses
/// must instead use [`super::pull_claimed_body`] to retain their durable claim.
///
/// # Errors
///
/// Preserves the transport failure as a source, or returns
/// [`AuthBodyError::TooLarge`] without retaining oversized response bytes.
pub async fn pull_auth_body(
    mut body: ResponseBody,
    maximum: usize,
) -> Result<Zeroizing<Vec<u8>>, AuthBodyError> {
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > maximum {
            let _ = body.close().await;
            return Err(AuthBodyError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Recognizes a JSON media type, ignoring parameters and ASCII case.
/// Header multiplicity policy remains with the caller.
#[must_use]
pub fn is_json_media_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_http::{
        Bytes, Client, ExchangeControl, HeaderMap, Limits, Method, Origin, Request,
    };
    use std::error::Error;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    async fn response_body(wire: &'static [u8]) -> Result<ResponseBody, Box<dyn Error>> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await?);
            }
            stream.write_all(wire).await?;
            stream.shutdown().await
        });
        let client = Client::new_http1(Limits::default())?;
        let request = Request::new(
            Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port())?,
            Method::GET,
            "/",
            HeaderMap::new(),
            Bytes::new(),
            client.limits(),
        )?;
        let response = client
            .execute(
                request,
                ExchangeControl::new(Instant::now() + Duration::from_secs(5)),
            )
            .await?;
        server.await??;
        Ok(response.into_body())
    }

    #[tokio::test]
    async fn body_bound_and_transport_source_are_preserved() -> Result<(), Box<dyn Error>> {
        const COMPLETE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata";
        let bytes = pull_auth_body(response_body(COMPLETE).await?, 4).await?;
        assert_eq!(bytes.as_slice(), b"data");
        let oversized = pull_auth_body(response_body(COMPLETE).await?, 3).await;
        assert!(matches!(oversized, Err(AuthBodyError::TooLarge)));
        let truncated = response_body(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\ndata").await?;
        let failed = pull_auth_body(truncated, 5).await;
        let Err(error @ AuthBodyError::Failure(_)) = failed else {
            return Err("expected transport failure for incomplete body".into());
        };
        assert!(error.source().is_some());
        assert!(!error.to_string().contains("data"));
        Ok(())
    }

    #[test]
    fn media_type_matching_does_not_accept_neighbors() {
        for value in ["application/json", " Application/JSON ; charset=utf-8"] {
            assert!(is_json_media_type(value));
        }
        for value in [
            "",
            "application/json-patch+json",
            "text/json",
            "application/json,",
        ] {
            assert!(!is_json_media_type(value));
        }
    }
}
