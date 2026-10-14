//! Framing helpers layered on [`plakous_http::SseDecoder`].

/// Returns whether unfinished physical line bytes are empty or comment-only.
///
/// Dialects that require blank-line frame termination treat comment-only
/// residual lines as idle (not truncated). Non-comment unfinished lines are
/// truncated frames.
#[must_use]
pub fn is_comment_only_unfinished(line: &[u8]) -> bool {
    if line.is_empty() {
        return true;
    }
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line.is_empty() || line.starts_with(b":")
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_comment_lines_are_idle() {
        assert!(is_comment_only_unfinished(b""));
        assert!(is_comment_only_unfinished(b":"));
        assert!(is_comment_only_unfinished(b": keep-alive"));
        assert!(is_comment_only_unfinished(b": keep-alive\r"));
        assert!(is_comment_only_unfinished(b"\r"));
    }

    #[test]
    fn data_prefix_is_not_idle() {
        assert!(!is_comment_only_unfinished(b"data:"));
        assert!(!is_comment_only_unfinished(b"d"));
        assert!(!is_comment_only_unfinished(b"data: x\r"));
    }
}
