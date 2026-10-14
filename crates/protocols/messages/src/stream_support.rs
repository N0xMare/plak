//! Bounded Messages stream grammar and cumulative wire accounting.

use std::num::NonZeroUsize;

use plakous_model::Usage;

/// Requires a named SSE event that exactly matches its JSON discriminator.
pub fn validate_named_event(event: Option<&str>, json_kind: &str) -> Result<(), &'static str> {
    match event {
        Some(name) if name == json_kind => Ok(()),
        Some(_) => Err("SSE event name disagrees with JSON event type"),
        None => Err("Messages SSE event name is required"),
    }
}

/// Configurable nonzero resource limits for one Messages stream.
#[allow(
    clippy::struct_field_names,
    reason = "every independent resource ceiling is explicit"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessagesStreamLimits {
    max_sse_line_bytes: NonZeroUsize,
    max_sse_event_bytes: NonZeroUsize,
    max_events: NonZeroUsize,
    max_blocks: NonZeroUsize,
    max_block_bytes: NonZeroUsize,
    max_total_stream_bytes: NonZeroUsize,
    max_unknown_event_bytes: NonZeroUsize,
}

impl MessagesStreamLimits {
    /// Creates an exact bounded family parser policy.
    pub const fn new(
        max_sse_line_bytes: NonZeroUsize,
        max_sse_event_bytes: NonZeroUsize,
        max_events: NonZeroUsize,
        max_blocks: NonZeroUsize,
        max_block_bytes: NonZeroUsize,
        max_total_stream_bytes: NonZeroUsize,
        max_unknown_event_bytes: NonZeroUsize,
    ) -> Self {
        Self {
            max_sse_line_bytes,
            max_sse_event_bytes,
            max_events,
            max_blocks,
            max_block_bytes,
            max_total_stream_bytes,
            max_unknown_event_bytes,
        }
    }
    /// Maximum SSE line bytes.
    pub const fn max_sse_line_bytes(self) -> usize {
        self.max_sse_line_bytes.get()
    }
    /// Maximum assembled SSE event bytes.
    pub const fn max_sse_event_bytes(self) -> usize {
        self.max_sse_event_bytes.get()
    }
    /// Maximum events.
    pub const fn max_events(self) -> usize {
        self.max_events.get()
    }
    /// Maximum content blocks.
    pub const fn max_blocks(self) -> usize {
        self.max_blocks.get()
    }
    /// Maximum bytes accumulated in one block.
    pub const fn max_block_bytes(self) -> usize {
        self.max_block_bytes.get()
    }
    /// Maximum total wire bytes.
    pub const fn max_total_stream_bytes(self) -> usize {
        self.max_total_stream_bytes.get()
    }
    /// Maximum retained bytes for one additive unknown event.
    pub const fn max_unknown_event_bytes(self) -> usize {
        self.max_unknown_event_bytes.get()
    }
}

impl Default for MessagesStreamLimits {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(4 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(100_000).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(4_096).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(16 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(64 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(256 * 1024).unwrap_or(NonZeroUsize::MIN),
        )
    }
}

/// Closed reviewed top-level Messages event grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessagesEventKind {
    /// Initial response metadata and input usage.
    MessageStart,
    /// Start of one indexed content block.
    ContentBlockStart,
    /// Delta for the currently open block.
    ContentBlockDelta,
    /// End of the currently open block.
    ContentBlockStop,
    /// Terminal metadata and cumulative usage update.
    MessageDelta,
    /// Successful stream terminator.
    MessageStop,
    /// Harmless bounded keepalive observation.
    Ping,
    /// In-band provider error.
    Error,
    /// Known but excluded citation semantic.
    Citation,
    /// Additive unknown top-level observation.
    Unknown,
}

impl MessagesEventKind {
    /// Classifies a top-level discriminator without inventing semantics.
    pub fn classify(value: &str) -> Self {
        match value {
            "message_start" => Self::MessageStart,
            "content_block_start" => Self::ContentBlockStart,
            "content_block_delta" => Self::ContentBlockDelta,
            "content_block_stop" => Self::ContentBlockStop,
            "message_delta" => Self::MessageDelta,
            "message_stop" => Self::MessageStop,
            "ping" => Self::Ping,
            "error" => Self::Error,
            "citation" | "citations_delta" => Self::Citation,
            _ => Self::Unknown,
        }
    }
}

/// Exact cumulative Messages usage counters before canonical normalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessagesProviderUsage {
    /// Ordinary input tokens.
    pub input_tokens: u64,
    /// Output tokens, including hidden thinking as reported.
    pub output_tokens: u64,
    /// Hidden thinking tokens included in `output_tokens`.
    pub thinking_tokens: u64,
    /// Cache-write input tokens.
    pub cache_creation_input_tokens: u64,
    /// Cache-read input tokens.
    pub cache_read_input_tokens: u64,
}

/// Optional cumulative fields from one terminal usage update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessagesUsageUpdate {
    /// Updated ordinary input tokens when emitted.
    pub input_tokens: Option<u64>,
    /// Required cumulative output tokens.
    pub output_tokens: u64,
    /// Updated cumulative hidden thinking tokens when emitted.
    pub thinking_tokens: Option<u64>,
    /// Updated cache-write input tokens when emitted.
    pub cache_creation_input_tokens: Option<u64>,
    /// Updated cache-read input tokens when emitted.
    pub cache_read_input_tokens: Option<u64>,
}

impl MessagesProviderUsage {
    /// Converts disjoint provider input counters to canonical usage, checking overflow.
    pub fn into_canonical(self) -> Result<Usage, &'static str> {
        if self.thinking_tokens > self.output_tokens {
            return Err("Messages thinking usage exceeded output usage");
        }
        let input = self
            .input_tokens
            .checked_add(self.cache_creation_input_tokens)
            .and_then(|total| total.checked_add(self.cache_read_input_tokens))
            .ok_or("Messages input usage counters overflowed canonical accounting")?;
        Ok(Usage::new(
            input,
            self.output_tokens,
            self.thinking_tokens,
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
        ))
    }

    /// Applies one cumulative update and rejects every decreasing counter.
    pub fn merge(self, update: MessagesUsageUpdate) -> Result<Self, &'static str> {
        let next = Self {
            input_tokens: update.input_tokens.unwrap_or(self.input_tokens),
            output_tokens: update.output_tokens,
            thinking_tokens: update.thinking_tokens.unwrap_or(self.thinking_tokens),
            cache_creation_input_tokens: update
                .cache_creation_input_tokens
                .unwrap_or(self.cache_creation_input_tokens),
            cache_read_input_tokens: update
                .cache_read_input_tokens
                .unwrap_or(self.cache_read_input_tokens),
        };
        if next.input_tokens < self.input_tokens
            || next.output_tokens < self.output_tokens
            || next.thinking_tokens < self.thinking_tokens
            || next.cache_creation_input_tokens < self.cache_creation_input_tokens
            || next.cache_read_input_tokens < self.cache_read_input_tokens
        {
            Err("Messages cumulative usage decreased")
        } else {
            Ok(next)
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use super::MessagesProviderUsage;

    #[test]
    fn canonical_usage_preserves_total_input_and_projects_cache_write_tokens() {
        let canonical = MessagesProviderUsage {
            input_tokens: 10,
            output_tokens: 1,
            thinking_tokens: 0,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 3,
        }
        .into_canonical()
        .expect("canonical usage");

        assert_eq!(canonical.input_tokens(), 15);
        assert_eq!(canonical.cached_input_tokens(), 3);
        assert_eq!(canonical.cache_write_tokens(), 2);
    }
}
