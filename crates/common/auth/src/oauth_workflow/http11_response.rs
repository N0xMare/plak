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

/// How a loopback callback ended, as its browser result page reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopbackCallbackOutcome {
    /// The login completed.
    Complete,
    /// The callback itself was refused (state, shape, or provider error).
    Rejected,
    /// The callback was accepted but the login did not complete.
    Failed,
}

/// Dialect text and headers of the loopback callback result page.
#[derive(Clone, Copy, Debug)]
pub struct LoopbackCallbackPage<'a> {
    /// Body for [`LoopbackCallbackOutcome::Complete`] (`200 OK`).
    pub complete: &'a str,
    /// Body for [`LoopbackCallbackOutcome::Rejected`] (`400 Bad Request`).
    pub rejected: &'a str,
    /// Body for [`LoopbackCallbackOutcome::Failed`] (`400 Bad Request`).
    pub failed: &'a str,
    /// Headers after the fixed framing headers.
    pub headers: &'a [(&'a str, &'a str)],
}

impl LoopbackCallbackPage<'_> {
    /// The complete `HTTP/1.1` response for `outcome`.
    #[must_use]
    pub fn response_bytes(&self, outcome: LoopbackCallbackOutcome) -> Vec<u8> {
        let (status, reason, body) = match outcome {
            LoopbackCallbackOutcome::Complete => (200, "OK", self.complete),
            LoopbackCallbackOutcome::Rejected => (400, "Bad Request", self.rejected),
            LoopbackCallbackOutcome::Failed => (400, "Bad Request", self.failed),
        };
        format_http11_text_response(status, reason, body, self.headers).into_bytes()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{LoopbackCallbackOutcome, LoopbackCallbackPage, format_http11_text_response};

    #[test]
    fn callback_page_maps_each_outcome_to_its_status_and_body() {
        let page = LoopbackCallbackPage {
            complete: "done",
            rejected: "refused",
            failed: "broken",
            headers: &[("Cache-Control", "no-store")],
        };
        for (outcome, head, body) in [
            (
                LoopbackCallbackOutcome::Complete,
                "HTTP/1.1 200 OK\r\n",
                "done",
            ),
            (
                LoopbackCallbackOutcome::Rejected,
                "HTTP/1.1 400 Bad Request\r\n",
                "refused",
            ),
            (
                LoopbackCallbackOutcome::Failed,
                "HTTP/1.1 400 Bad Request\r\n",
                "broken",
            ),
        ] {
            let response = String::from_utf8(page.response_bytes(outcome)).unwrap();
            assert!(response.starts_with(head), "{response}");
            assert!(response.contains("\r\nCache-Control: no-store\r\n"));
            assert!(response.ends_with(&format!("\r\n\r\n{body}")));
        }
    }

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
