//! Provider-blind SSE **framing** (byte stream → complete event messages).
//!
//! This module owns HTML5 Server-Sent Events field grammar only: line endings,
//! comments, `event`/`data` fields, multi-line data join, and size limits.
//! It does **not** interpret JSON, `[DONE]`, sequences, or provider event types.
//!

use thiserror::Error;

/// Hard limits for incremental SSE framing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SseLimits {
    /// Maximum bytes retained for one physical SSE line.
    pub max_line_bytes: usize,
    /// Maximum joined data bytes retained for one SSE event.
    pub max_event_bytes: usize,
}

impl Default for SseLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: 1024 * 1024,
            max_event_bytes: 4 * 1024 * 1024,
        }
    }
}

/// One completely framed SSE message.
#[derive(Clone, Eq, PartialEq)]
pub struct SseMessage {
    event: Option<String>,
    data: String,
}

impl std::fmt::Debug for SseMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SseMessage")
            .field("event", &self.event)
            .field("data", &"<redacted>")
            .field("data_bytes", &self.data.len())
            .finish()
    }
}

impl SseMessage {
    /// Returns the optional `event` field.
    #[must_use]
    pub fn event(&self) -> Option<&str> {
        self.event.as_deref()
    }

    /// Returns data lines joined with a single newline.
    #[must_use]
    pub fn data(&self) -> &str {
        &self.data
    }
}

/// Framing errors for [`SseDecoder`].
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SseError {
    /// An SSE line was not valid UTF-8.
    #[error("SSE data is not valid UTF-8")]
    InvalidUtf8,
    /// An SSE line or joined event exceeded configured limits.
    #[error("SSE resource limit exceeded: {0}")]
    ResourceLimit(&'static str),
    /// Further bytes after a terminal framing error.
    #[error("SSE decoder already failed")]
    AlreadyFailed,
}

/// A fragmentation-independent, UTF-8 validating SSE frame decoder.
#[derive(Clone)]
pub struct SseDecoder {
    limits: SseLimits,
    line: Vec<u8>,
    event: Option<String>,
    data_lines: Vec<String>,
    data_bytes: usize,
    /// When true, a CR was just consumed as a line ending; a following LF is ignored (CRLF).
    pending_lf: bool,
    failed: bool,
}

impl std::fmt::Debug for SseDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SseDecoder")
            .field("limits", &self.limits)
            .field("line_bytes", &self.line.len())
            .field("event", &self.event)
            .field("data", &"<redacted>")
            .field("data_bytes", &self.data_bytes)
            .field("pending_lf", &self.pending_lf)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl SseDecoder {
    /// Creates a decoder with explicit resource limits.
    #[must_use]
    pub fn new(limits: SseLimits) -> Self {
        Self {
            limits,
            line: Vec::new(),
            event: None,
            data_lines: Vec::new(),
            data_bytes: 0,
            pending_lf: false,
            failed: false,
        }
    }

    /// Feeds an arbitrary byte fragment and returns every complete message.
    ///
    /// Line endings follow the HTML5 SSE grammar: CR, LF, or CRLF.
    ///
    /// # Errors
    ///
    /// Returns a terminal error for invalid UTF-8 or configured size limits.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseMessage>, SseError> {
        if self.failed {
            return Err(SseError::AlreadyFailed);
        }
        let mut messages = Vec::new();
        for &byte in bytes {
            if self.pending_lf {
                self.pending_lf = false;
                if byte == b'\n' {
                    // CRLF: line already finished on CR.
                    continue;
                }
                // Lone CR was the ending; this byte begins the next line.
            }
            match byte {
                b'\n' => {
                    if let Err(error) = self.finish_line(&mut messages) {
                        self.failed = true;
                        return Err(error);
                    }
                }
                b'\r' => {
                    if let Err(error) = self.finish_line(&mut messages) {
                        self.failed = true;
                        return Err(error);
                    }
                    self.pending_lf = true;
                }
                _ => {
                    self.line.push(byte);
                    if self.line.len() > self.limits.max_line_bytes {
                        self.failed = true;
                        return Err(SseError::ResourceLimit("SSE line bytes"));
                    }
                }
            }
        }
        Ok(messages)
    }

    /// Returns whether the decoder holds no unfinished line or undelivered fields.
    ///
    /// Dialects that require blank-line frame termination (for example xAI and
    /// `OpenRouter` Responses) use this instead of auto-dispatching residual data
    /// at EOF.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.line.is_empty() && self.data_lines.is_empty() && self.event.is_none()
    }

    /// Returns unfinished physical line bytes retained between newline delimiters.
    #[must_use]
    pub fn unfinished_line(&self) -> &[u8] {
        &self.line
    }

    /// Returns whether event or data fields are staged without a blank-line dispatch.
    #[must_use]
    pub fn has_undelivered_event(&self) -> bool {
        !self.data_lines.is_empty() || self.event.is_some()
    }

    /// Finishes the byte stream and dispatches a final unterminated event.
    ///
    /// # Errors
    ///
    /// Returns a terminal framing error for invalid residual data.
    pub fn finish(&mut self) -> Result<Vec<SseMessage>, SseError> {
        if self.failed {
            return Err(SseError::AlreadyFailed);
        }
        let mut messages = Vec::new();
        let line_result = if self.line.is_empty() {
            Ok(())
        } else {
            self.finish_line(&mut messages)
        };
        if let Err(error) = line_result {
            self.failed = true;
            return Err(error);
        }
        self.dispatch(&mut messages);
        Ok(messages)
    }

    fn finish_line(&mut self, messages: &mut Vec<SseMessage>) -> Result<(), SseError> {
        let line_bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&line_bytes).map_err(|_| SseError::InvalidUtf8)?;
        if line.is_empty() {
            self.dispatch(messages);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, mut value) = line.split_once(':').unwrap_or((line, ""));
        if let Some(stripped) = value.strip_prefix(' ') {
            value = stripped;
        }
        match field {
            "event" => {
                // Empty event names are treated as absent (HTML5 ignores empty names).
                self.event = (!value.is_empty()).then(|| value.to_owned());
            }
            "data" => {
                self.data_bytes = self
                    .data_bytes
                    .checked_add(value.len())
                    .and_then(|total| total.checked_add(usize::from(!self.data_lines.is_empty())))
                    .ok_or(SseError::ResourceLimit("SSE event bytes"))?;
                if self.data_bytes > self.limits.max_event_bytes {
                    return Err(SseError::ResourceLimit("SSE event bytes"));
                }
                self.data_lines.push(value.to_owned());
            }
            // `id`, `retry`, and unknown extension fields are ignored by framing.
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, messages: &mut Vec<SseMessage>) {
        if self.data_lines.is_empty() {
            self.event = None;
            self.data_bytes = 0;
            return;
        }
        let data = self.data_lines.join("\n");
        messages.push(SseMessage {
            event: self.event.take(),
            data,
        });
        self.data_lines.clear();
        self.data_bytes = 0;
    }
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
    fn framing_is_fragmentation_independent_and_joins_data_lines() {
        let frames = b"event: x\ndata: one\ndata: \xF0\x9F\x8C\x8D\n\ndata: two\n\n";
        let mut baseline = SseDecoder::new(SseLimits::default());
        let expected = baseline.push(frames).expect("frame");
        assert_eq!(expected.len(), 2);
        assert_eq!(expected[0].event(), Some("x"));
        assert_eq!(expected[0].data(), "one\n🌍");
        assert_eq!(expected[1].data(), "two");
        for split in 0..=frames.len() {
            let mut decoder = SseDecoder::new(SseLimits::default());
            let mut got = decoder.push(&frames[..split]).expect("prefix");
            got.extend(decoder.push(&frames[split..]).expect("suffix"));
            got.extend(decoder.finish().expect("finish"));
            assert_eq!(got, expected);
        }
        let mut invalid = SseDecoder::new(SseLimits::default());
        assert!(matches!(
            invalid.push(b"data: \xFF\n\n"),
            Err(SseError::InvalidUtf8)
        ));
        assert!(matches!(
            invalid.push(b"data: more\n\n"),
            Err(SseError::AlreadyFailed)
        ));
        let mut invalid_at_finish = SseDecoder::new(SseLimits::default());
        assert_eq!(
            invalid_at_finish.push(b"data: ok\n").unwrap(),
            [] as [SseMessage; 0]
        );
        assert!(matches!(
            invalid_at_finish.finish(),
            Ok(messages) if messages.len() == 1 && messages[0].data() == "ok"
        ));
    }

    #[test]
    fn cr_lf_and_crlf_line_endings_are_equivalent() {
        let lf = b"data: a\n\ndata: b\n\n";
        let cr = b"data: a\r\rdata: b\r\r";
        let crlf = b"data: a\r\n\r\ndata: b\r\n\r\n";
        let mut lf_decoder = SseDecoder::new(SseLimits::default());
        let expected = lf_decoder.push(lf).expect("lf");
        assert_eq!(expected.len(), 2);
        for frames in [cr.as_slice(), crlf.as_slice()] {
            let mut decoder = SseDecoder::new(SseLimits::default());
            assert_eq!(decoder.push(frames).expect("push"), expected);
        }
    }

    #[test]
    fn comments_and_empty_dispatch_without_data_are_dropped() {
        let mut decoder = SseDecoder::new(SseLimits::default());
        let messages = decoder
            .push(b": keep-alive\nevent: only\n\ndata: real\n\n")
            .expect("push");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data(), "real");
    }

    #[test]
    fn line_byte_limit_is_enforced() {
        let mut decoder = SseDecoder::new(SseLimits {
            max_line_bytes: 8,
            max_event_bytes: 1024,
        });
        assert!(matches!(
            decoder.push(b"data: 123456789\n"),
            Err(SseError::ResourceLimit("SSE line bytes"))
        ));
    }
}
