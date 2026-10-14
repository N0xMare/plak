//! Stream / complete-response resource limits shared by Responses-family dialects.

/// Hard limits for one Responses-family SSE decode operation.
///
/// Frame limits are applied to both physical SSE lines and joined event data
/// (same policy as current commercial dialect decoders).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesStreamLimits {
    /// Maximum bytes in one SSE line or joined event frame.
    pub max_frame_bytes: usize,
    /// Maximum total bytes accepted for one stream operation.
    pub max_total_bytes: usize,
    /// Maximum decoded JSON events (excludes `[DONE]` and empty frames).
    pub max_events: usize,
}

impl ResponsesStreamLimits {
    /// Creates limits with the given positive values.
    ///
    /// # Errors
    ///
    /// Returns [`ResponsesStreamLimitError::Invalid`] when any limit is zero or
    /// `max_frame_bytes` exceeds `max_total_bytes`.
    pub const fn new(
        max_frame_bytes: usize,
        max_total_bytes: usize,
        max_events: usize,
    ) -> Result<Self, ResponsesStreamLimitError> {
        if max_frame_bytes == 0
            || max_total_bytes == 0
            || max_events == 0
            || max_frame_bytes > max_total_bytes
        {
            return Err(ResponsesStreamLimitError::Invalid);
        }
        Ok(Self {
            max_frame_bytes,
            max_total_bytes,
            max_events,
        })
    }

    /// Returns the maximum frame (line / event) byte budget.
    #[must_use]
    pub const fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes
    }

    /// Returns the maximum total stream byte budget.
    #[must_use]
    pub const fn max_total_bytes(self) -> usize {
        self.max_total_bytes
    }

    /// Returns the maximum decoded JSON event count.
    #[must_use]
    pub const fn max_events(self) -> usize {
        self.max_events
    }

    /// Converts these limits into shared SSE framing limits.
    #[must_use]
    pub const fn sse_limits(self) -> plakous_http::SseLimits {
        plakous_http::SseLimits {
            max_line_bytes: self.max_frame_bytes,
            max_event_bytes: self.max_frame_bytes,
        }
    }
}

impl Default for ResponsesStreamLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 2 * 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
            max_events: 100_000,
        }
    }
}

/// Invalid [`ResponsesStreamLimits`] configuration.
#[derive(Clone, Copy, Debug, Eq, thiserror::Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesStreamLimitError {
    /// A limit was zero or the frame limit exceeded the total byte limit.
    #[error("Responses stream limits must be positive and frame bytes cannot exceed total bytes")]
    Invalid,
}

/// Hard limits shared by complete-response and stream decoders (xAI / OpenRouter).
///
/// Extends stream budgets with a maximum output-item count used for
/// `output_index` admission and complete-response size checks.
#[allow(
    clippy::struct_field_names,
    reason = "the max prefix makes every public resource limit unambiguous"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesDecodeLimits {
    max_frame_bytes: usize,
    max_total_bytes: usize,
    max_events: usize,
    max_output_items: usize,
}

impl ResponsesDecodeLimits {
    /// Creates a positive, internally consistent limit set.
    ///
    /// # Errors
    ///
    /// Returns [`ResponsesDecodeLimitError::Invalid`] if any limit is zero or a
    /// frame may exceed the total operation byte limit.
    pub const fn new(
        max_frame_bytes: usize,
        max_total_bytes: usize,
        max_events: usize,
        max_output_items: usize,
    ) -> Result<Self, ResponsesDecodeLimitError> {
        if max_frame_bytes == 0
            || max_total_bytes == 0
            || max_events == 0
            || max_output_items == 0
            || max_frame_bytes > max_total_bytes
        {
            return Err(ResponsesDecodeLimitError::Invalid);
        }
        Ok(Self {
            max_frame_bytes,
            max_total_bytes,
            max_events,
            max_output_items,
        })
    }

    /// Returns the maximum bytes in one SSE event frame.
    #[must_use]
    pub const fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes
    }

    /// Returns the maximum total bytes in one complete response or SSE operation.
    #[must_use]
    pub const fn max_total_bytes(self) -> usize {
        self.max_total_bytes
    }

    /// Returns the maximum decoded SSE events in one operation.
    #[must_use]
    pub const fn max_events(self) -> usize {
        self.max_events
    }

    /// Returns the maximum output items / valid `output_index` range.
    #[must_use]
    pub const fn max_output_items(self) -> usize {
        self.max_output_items
    }

    /// Projects the stream subset used by [`ResponsesStreamLimits`].
    #[must_use]
    pub const fn stream_limits(self) -> ResponsesStreamLimits {
        ResponsesStreamLimits {
            max_frame_bytes: self.max_frame_bytes,
            max_total_bytes: self.max_total_bytes,
            max_events: self.max_events,
        }
    }
}

impl Default for ResponsesDecodeLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 2 * 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
            max_events: 100_000,
            max_output_items: 4_096,
        }
    }
}

/// Invalid [`ResponsesDecodeLimits`] configuration.
#[derive(Clone, Copy, Debug, Eq, thiserror::Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesDecodeLimitError {
    /// A limit was zero or the frame limit exceeded the total byte limit.
    #[error("Responses decoder limits must be positive and frame bytes cannot exceed total bytes")]
    Invalid,
}
