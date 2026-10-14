//! Pure HTTP/1.1 plain-text response framing for OAuth loopback callbacks.

/// Formats `HTTP/1.1` text response: Content-Type, Content-Length, Connection,
/// then `extra_headers`, then body.
#[must_use]
pub fn format_http11_text_response(
    status_code: u16,
    reason: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> String {
    let mut response = format!(
        "HTTP/1.1 {status_code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    response.push_str(body);
    response
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::format_http11_text_response;

    #[test]
    fn formats_success_and_failure_with_extra_headers() {
        let ok = format_http11_text_response(
            200,
            "OK",
            "Login complete",
            &[("Cache-Control", "no-store")],
        );
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(ok.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(ok.contains("Content-Length: 14\r\n"));
        assert!(ok.contains("Connection: close\r\n"));
        assert!(ok.contains("Cache-Control: no-store\r\n"));
        assert!(ok.ends_with("\r\n\r\nLogin complete"));

        let bad = format_http11_text_response(400, "Bad Request", "Login failed", &[]);
        assert!(bad.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(bad.ends_with("\r\n\r\nLogin failed"));
        assert!(!bad.contains("Cache-Control"));
    }
}
