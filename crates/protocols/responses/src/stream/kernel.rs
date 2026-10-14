//! Front-half Responses-family stream kernel.
//!
//! Owns: SSE framing, optional BOM strip, total-byte/event limits, sequence
//! tracking, `[DONE]` / typed-terminal policy, truncated-frame finish checks.
//!
//! Does **not** own: dialect JSON event shapes, response lifecycle correlation,
//! provider error types, or model-facing policy.

use plakous_http::{SseDecoder, SseMessage};
use serde_json::Value;

use crate::dialect::{
    DoneMarkerDecision, FinishPolicy, ResponsesDialectProfile, SequencePolicy, SequenceState,
    TerminalPolicy, decide_done_marker, terminal_finish_ok, validate_next_sequence,
};
use crate::json_fields::{ResponsesFieldError, optional_u64_fields};

use super::bom::BomLead;
use super::error::resource;
use super::framing::is_comment_only_unfinished;
use super::hooks::DialectStreamHooks;
use super::limits::ResponsesStreamLimits;

/// Shared Responses-family SSE stream lifecycle (front-half kernel).
///
/// Dialects hold plugin state outside this struct and pass
/// [`DialectStreamHooks`] into [`Self::push`] / [`Self::finish`].
#[derive(Clone)]
pub struct ResponsesStreamKernel {
    profile: ResponsesDialectProfile,
    sse: SseDecoder,
    sequence: SequenceState,
    typed_terminal_seen: bool,
    done_marker_seen: bool,
    failed: bool,
    total_bytes: usize,
    event_count: usize,
    limits: ResponsesStreamLimits,
    bom_lead: BomLead,
    typed_terminal_cutoff: bool,
}

impl std::fmt::Debug for ResponsesStreamKernel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResponsesStreamKernel")
            .field("profile", &self.profile)
            .field("sse_idle", &self.sse.is_idle())
            .field("sequence", &self.sequence)
            .field("typed_terminal_seen", &self.typed_terminal_seen)
            .field("done_marker_seen", &self.done_marker_seen)
            .field("failed", &self.failed)
            .field("total_bytes", &self.total_bytes)
            .field("event_count", &self.event_count)
            .field("limits", &self.limits)
            .field("bom_pending", &self.bom_lead.has_pending_bytes())
            .finish_non_exhaustive()
    }
}

impl ResponsesStreamKernel {
    /// Creates a kernel for one dialect profile and stream limits.
    #[must_use]
    pub fn new(profile: ResponsesDialectProfile, limits: ResponsesStreamLimits) -> Self {
        Self {
            profile,
            sse: SseDecoder::new(limits.sse_limits()),
            sequence: SequenceState::new(),
            typed_terminal_seen: false,
            done_marker_seen: false,
            failed: false,
            total_bytes: 0,
            event_count: 0,
            limits,
            bom_lead: BomLead::for_policy(profile.bom()),
            typed_terminal_cutoff: false,
        }
    }

    /// Stops semantic decoding at the first valid typed terminal.
    ///
    /// This is an explicit dialect behavior used by the Codex and xAI
    /// clients, whose reference transports drop the SSE body as soon as
    /// `response.completed` is accepted.
    #[must_use]
    pub const fn with_typed_terminal_cutoff(mut self) -> Self {
        self.typed_terminal_cutoff = true;
        self
    }

    /// Returns the dialect profile fixed for this kernel.
    #[must_use]
    pub const fn profile(&self) -> ResponsesDialectProfile {
        self.profile
    }

    /// Returns the stream resource limits.
    #[must_use]
    pub const fn limits(&self) -> ResponsesStreamLimits {
        self.limits
    }

    /// Returns whether the kernel is terminally failed.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        self.failed
    }

    /// Returns whether a typed terminal event was accepted.
    #[must_use]
    pub const fn typed_terminal_seen(&self) -> bool {
        self.typed_terminal_seen
    }

    /// Returns whether a `[DONE]` marker was accepted.
    #[must_use]
    pub const fn done_marker_seen(&self) -> bool {
        self.done_marker_seen
    }

    /// Returns total accepted stream bytes so far.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Returns the count of decoded JSON events so far.
    #[must_use]
    pub const fn event_count(&self) -> usize {
        self.event_count
    }

    /// Returns the sequence tracker.
    #[must_use]
    pub const fn sequence(&self) -> &SequenceState {
        &self.sequence
    }

    /// Returns whether the underlying SSE framer holds no unfinished work.
    #[must_use]
    pub fn sse_is_idle(&self) -> bool {
        self.sse.is_idle()
    }

    /// Marks the kernel terminally failed (dialect outer transactional push).
    pub fn mark_failed(&mut self) {
        self.failed = true;
    }

    /// Feeds transport bytes and returns newly completed dialect events.
    ///
    /// # Errors
    ///
    /// Returns a dialect-mapped error for limits, framing, sequence, `[DONE]`,
    /// or hook decode failures. On error the kernel is marked failed.
    pub fn push<H: DialectStreamHooks>(
        &mut self,
        chunk: &[u8],
        hooks: &mut H,
    ) -> Result<Vec<H::Event>, H::Error> {
        if self.failed {
            return Err(H::map_sse_error(
                plakous_http::SseError::AlreadyFailed,
                &self.limits,
            ));
        }
        match self.push_inner(chunk, hooks) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn push_inner<H: DialectStreamHooks>(
        &mut self,
        chunk: &[u8],
        hooks: &mut H,
    ) -> Result<Vec<H::Event>, H::Error> {
        if self.content_closed() && self.typed_terminal_cutoff {
            return Ok(Vec::new());
        }
        if self.typed_terminal_cutoff {
            return self.push_to_typed_terminal(chunk, hooks);
        }
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());
        if self.total_bytes > self.limits.max_total_bytes() {
            return Err(H::map_resource_limit(
                resource::TOTAL_STREAM_BYTES,
                self.limits.max_total_bytes(),
            ));
        }
        let Some(feed) = self.bom_lead.take_bytes_after_bom(chunk) else {
            return Ok(Vec::new());
        };
        let messages = self
            .sse
            .push(&feed)
            .map_err(|error| H::map_sse_error(error, &self.limits))?;
        if self.content_closed() {
            if self.typed_terminal_cutoff {
                return Ok(Vec::new());
            }
            if !messages.is_empty()
                || self.sse.has_undelivered_event()
                || !is_comment_only_unfinished(self.sse.unfinished_line())
            {
                return Err(H::map_event_after_terminal());
            }
            return Ok(Vec::new());
        }
        let mut staged = Vec::new();
        for message in &messages {
            if let Some(event) = self.decode_message(message, hooks)? {
                self.event_count = self.event_count.saturating_add(1);
                if self.event_count > self.limits.max_events() {
                    return Err(H::map_resource_limit(
                        resource::SSE_EVENTS,
                        self.limits.max_events(),
                    ));
                }
                let terminal = H::event_is_terminal(&event);
                staged.push(event);
                // Codex and xAI treat the typed terminal as the event-stream
                // boundary and drop the response body at that point. Bytes
                // already buffered after that SSE event are therefore
                // transport trailers, not another semantic event. OpenRouter
                // keeps waiting for its required `[DONE]`.
                if terminal && self.typed_terminal_cutoff {
                    break;
                }
            }
        }
        Ok(staged)
    }

    fn push_to_typed_terminal<H: DialectStreamHooks>(
        &mut self,
        chunk: &[u8],
        hooks: &mut H,
    ) -> Result<Vec<H::Event>, H::Error> {
        let mut staged = Vec::new();
        let mut start = 0;
        for end in chunk
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| matches!(byte, b'\r' | b'\n').then_some(index + 1))
            .chain(std::iter::once(chunk.len()))
        {
            if end == start {
                continue;
            }
            let physical = &chunk[start..end];
            start = end;
            self.total_bytes = self.total_bytes.saturating_add(physical.len());
            if self.total_bytes > self.limits.max_total_bytes() {
                return Err(H::map_resource_limit(
                    resource::TOTAL_STREAM_BYTES,
                    self.limits.max_total_bytes(),
                ));
            }
            let Some(feed) = self.bom_lead.take_bytes_after_bom(physical) else {
                continue;
            };
            let messages = self
                .sse
                .push(&feed)
                .map_err(|error| H::map_sse_error(error, &self.limits))?;
            for message in &messages {
                if let Some(event) = self.decode_message(message, hooks)? {
                    self.event_count = self.event_count.saturating_add(1);
                    if self.event_count > self.limits.max_events() {
                        return Err(H::map_resource_limit(
                            resource::SSE_EVENTS,
                            self.limits.max_events(),
                        ));
                    }
                    let terminal = H::event_is_terminal(&event);
                    staged.push(event);
                    if terminal {
                        return Ok(staged);
                    }
                }
            }
        }
        Ok(staged)
    }

    /// Finishes the stream, optionally auto-dispatching residual frames, and
    /// verifies terminal policy.
    ///
    /// Residual handling follows [`FinishPolicy`]:
    /// - [`FinishPolicy::BlankLineClosed`]: undelivered fields / non-comment
    ///   unfinished lines are truncated (no residual events).
    /// - [`FinishPolicy::AutoDispatchResidual`]: residual frames are finished
    ///   like [`SseDecoder::finish`] and decoded through hooks.
    ///
    /// # Errors
    ///
    /// Returns truncated-frame, missing-terminal, missing-`[DONE]`, or residual
    /// decode errors mapped through hooks. On error the kernel is marked failed.
    pub fn finish<H: DialectStreamHooks>(
        &mut self,
        hooks: &mut H,
    ) -> Result<Vec<H::Event>, H::Error> {
        if self.failed {
            return Err(H::map_sse_error(
                plakous_http::SseError::AlreadyFailed,
                &self.limits,
            ));
        }
        match self.finish_inner(hooks) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn finish_inner<H: DialectStreamHooks>(
        &mut self,
        hooks: &mut H,
    ) -> Result<Vec<H::Event>, H::Error> {
        if self.typed_terminal_cutoff && self.typed_terminal_seen {
            return Ok(Vec::new());
        }
        if self.bom_lead.has_pending_bytes() {
            return Err(H::map_truncated());
        }
        let residual = match self.profile.finish() {
            FinishPolicy::BlankLineClosed => {
                // Blank-line frame termination: residual data/event fields or a
                // non-comment unfinished line is truncated, not auto-dispatched.
                if self.sse.has_undelivered_event()
                    || !is_comment_only_unfinished(self.sse.unfinished_line())
                {
                    return Err(H::map_truncated());
                }
                Vec::new()
            }
            FinishPolicy::AutoDispatchResidual => self
                .sse
                .finish()
                .map_err(|error| H::map_sse_error(error, &self.limits))?,
        };
        let mut staged = Vec::new();
        for message in &residual {
            if let Some(event) = self.decode_message(message, hooks)? {
                self.event_count = self.event_count.saturating_add(1);
                if self.event_count > self.limits.max_events() {
                    return Err(H::map_resource_limit(
                        resource::SSE_EVENTS,
                        self.limits.max_events(),
                    ));
                }
                staged.push(event);
            }
        }
        let terminal = self.profile.terminal();
        if !self.typed_terminal_seen {
            return Err(H::map_missing_terminal());
        }
        if matches!(terminal, TerminalPolicy::TypedThenDone) && !self.done_marker_seen {
            return Err(H::map_missing_done());
        }
        debug_assert!(terminal_finish_ok(
            terminal,
            self.typed_terminal_seen,
            self.done_marker_seen
        ));
        Ok(staged)
    }

    /// Whether the stream rejects further content at the push level.
    ///
    /// - [`TerminalPolicy::TypedOnly`]: after typed terminal (xAI).
    /// - [`TerminalPolicy::TypedThenDone`]: after accepted `[DONE]` (`OpenRouter`).
    /// - [`TerminalPolicy::TypedOptionalDone`]: never early-closed at the byte
    ///   API so fragmented optional DONE markers remain decodable. The
    ///   transport owner stops pulling at the typed terminal boundary; buffered
    ///   trailers in that same push are ignored by the event loop above.
    const fn content_closed(&self) -> bool {
        if self.typed_terminal_cutoff && self.typed_terminal_seen {
            return true;
        }
        match self.profile.terminal() {
            TerminalPolicy::TypedOnly => self.typed_terminal_seen,
            TerminalPolicy::TypedThenDone => self.done_marker_seen,
            TerminalPolicy::TypedOptionalDone => false,
        }
    }

    fn decode_message<H: DialectStreamHooks>(
        &mut self,
        message: &SseMessage,
        hooks: &mut H,
    ) -> Result<Option<H::Event>, H::Error> {
        let data = message.data();
        if data.is_empty() && message.event().is_none() {
            return Ok(None);
        }
        if data == "[DONE]" {
            return match decide_done_marker(
                self.profile.terminal(),
                self.typed_terminal_seen,
                self.done_marker_seen,
            ) {
                DoneMarkerDecision::Accept => {
                    self.done_marker_seen = true;
                    Ok(None)
                }
                DoneMarkerDecision::Forbidden | DoneMarkerDecision::OutOfOrder => {
                    Err(H::map_done_forbidden())
                }
            };
        }
        if self.typed_terminal_seen {
            return Err(H::map_event_after_terminal());
        }
        let event_name = message.event().filter(|name| !name.is_empty());
        let sequence = self.extract_and_validate_sequence(data, hooks)?;
        let Some(event) = hooks.decode_json_event(data, event_name, sequence)? else {
            return Ok(None);
        };
        if H::event_is_terminal(&event) {
            self.typed_terminal_seen = true;
        }
        Ok(Some(event))
    }

    /// Extracts sequence from JSON when possible and validates under the profile.
    ///
    /// Non-object / non-JSON payloads skip kernel sequence validation so dialect
    /// hooks can emit their native JSON errors. Field-type failures on optional
    /// sequence keys fail closed via [`DialectStreamHooks::map_field_error`].
    fn extract_and_validate_sequence<H: DialectStreamHooks>(
        &mut self,
        data: &str,
        _hooks: &H,
    ) -> Result<Option<u64>, H::Error> {
        match peek_sequence(self.profile.sequence(), data) {
            SequencePeek::NotObject => Ok(None),
            SequencePeek::FieldError(error) => Err(H::map_field_error(error)),
            SequencePeek::Value(sequence) => {
                validate_next_sequence(self.profile.sequence(), &mut self.sequence, sequence)
                    .map_err(H::map_sequence_error)?;
                Ok(sequence)
            }
        }
    }
}

enum SequencePeek {
    NotObject,
    FieldError(ResponsesFieldError),
    Value(Option<u64>),
}

fn peek_sequence(policy: SequencePolicy, data: &str) -> SequencePeek {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return SequencePeek::NotObject;
    };
    let Some(object) = value.as_object() else {
        return SequencePeek::NotObject;
    };
    match policy {
        SequencePolicy::RequiredStrict => {
            // Match OpenAI Platform: wrong JSON type is treated as missing.
            SequencePeek::Value(object.get("sequence_number").and_then(Value::as_u64))
        }
        SequencePolicy::OptionalMonotonic { fields } => match optional_u64_fields(object, fields) {
            Ok(sequence) => SequencePeek::Value(sequence),
            Err(error) => SequencePeek::FieldError(error),
        },
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
    use crate::stream::hooks::DialectStreamHooks;
    use plakous_http::SseError;

    #[derive(Debug, PartialEq)]
    enum TestError {
        Sse,
        Sequence,
        Done,
        AfterTerminal,
        Truncated,
        MissingTerminal,
        MissingDone,
        Limit(&'static str),
        Decode(&'static str),
    }

    #[derive(Debug, PartialEq)]
    struct TestEvent {
        terminal: bool,
        data: String,
    }

    struct TestHooks {
        fail_decode: bool,
    }

    impl DialectStreamHooks for TestHooks {
        type Event = TestEvent;
        type Error = TestError;

        fn map_sse_error(_err: SseError, _limits: &ResponsesStreamLimits) -> Self::Error {
            TestError::Sse
        }

        fn map_sequence_error(_err: crate::SequenceValidationError) -> Self::Error {
            TestError::Sequence
        }

        fn map_done_forbidden() -> Self::Error {
            TestError::Done
        }

        fn map_event_after_terminal() -> Self::Error {
            TestError::AfterTerminal
        }

        fn map_truncated() -> Self::Error {
            TestError::Truncated
        }

        fn map_missing_terminal() -> Self::Error {
            TestError::MissingTerminal
        }

        fn map_missing_done() -> Self::Error {
            TestError::MissingDone
        }

        fn map_resource_limit(resource: &'static str, _maximum: usize) -> Self::Error {
            TestError::Limit(resource)
        }

        fn map_field_error(_err: ResponsesFieldError) -> Self::Error {
            TestError::Decode("field")
        }

        fn event_is_terminal(event: &Self::Event) -> bool {
            event.terminal
        }

        fn decode_json_event(
            &mut self,
            data: &str,
            _event_name: Option<&str>,
            _sequence: Option<u64>,
        ) -> Result<Option<Self::Event>, Self::Error> {
            if self.fail_decode {
                return Err(TestError::Decode("hook"));
            }
            let value: Value = serde_json::from_str(data).map_err(|_| TestError::Decode("json"))?;
            let terminal = value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "done");
            Ok(Some(TestEvent {
                terminal,
                data: data.to_owned(),
            }))
        }
    }

    fn frame(json: &str) -> Vec<u8> {
        format!("data: {json}\n\n").into_bytes()
    }

    #[test]
    fn xai_profile_forbids_done_and_requires_typed_terminal() {
        let mut kernel = ResponsesStreamKernel::new(
            ResponsesDialectProfile::xai(),
            ResponsesStreamLimits::default(),
        );
        let mut hooks = TestHooks { fail_decode: false };
        let events = kernel
            .push(&frame(r#"{"type":"done","sequence_number":0}"#), &mut hooks)
            .unwrap_or_else(|error| panic!("push: {error:?}"));
        assert_eq!(events.len(), 1);
        assert!(kernel.typed_terminal_seen());
        assert!(matches!(
            kernel.push(b"data: [DONE]\n\n", &mut hooks),
            Err(TestError::AfterTerminal | TestError::Done)
        ));
    }

    #[test]
    fn openrouter_profile_requires_done_after_terminal() {
        let mut kernel = ResponsesStreamKernel::new(
            ResponsesDialectProfile::openrouter(),
            ResponsesStreamLimits::default(),
        );
        let mut hooks = TestHooks { fail_decode: false };
        assert!(
            kernel
                .push(&frame(r#"{"type":"done","sequence_number":0}"#), &mut hooks)
                .is_ok()
        );
        let mut missing_done = kernel.clone();
        assert!(matches!(
            missing_done.finish(&mut hooks),
            Err(TestError::MissingDone)
        ));
        assert!(kernel.push(b"data: [DONE]\n\n", &mut hooks).is_ok());
        assert!(kernel.finish(&mut hooks).is_ok());
    }

    #[test]
    fn openai_terminal_closes_the_event_boundary_and_ignores_buffered_trailers() {
        let mut kernel = ResponsesStreamKernel::new(
            ResponsesDialectProfile::openai_platform(),
            ResponsesStreamLimits::default(),
        )
        .with_typed_terminal_cutoff();
        let mut hooks = TestHooks { fail_decode: false };
        let bytes = concat!(
            "data: {\"type\":\"done\",\"sequence_number\":0}\n\n",
            "data: {\"type\":\"opaque.provider.trailer\",\"sequence_number\":1}\n\n",
            "data: [DONE]\n\n"
        );
        let events = kernel
            .push(bytes.as_bytes(), &mut hooks)
            .unwrap_or_else(|error| panic!("push: {error:?}"));
        assert_eq!(events.len(), 1);
        assert!(events[0].terminal);
        assert!(kernel.typed_terminal_seen());
        assert!(!kernel.done_marker_seen());
        assert!(kernel.finish(&mut hooks).is_ok());
    }

    #[test]
    fn openai_terminal_cutoff_ignores_malformed_and_over_limit_same_chunk_suffix() {
        let terminal = b"data: {\"type\":\"done\",\"sequence_number\":0}\n\n";
        let limits = ResponsesStreamLimits::new(64, 256, 16)
            .unwrap_or_else(|error| panic!("limits: {error}"));
        for suffix in [
            &[0xff, b'\n', b'\n'][..],
            &vec![b'x'; limits.max_total_bytes() + 1],
        ] {
            let mut kernel =
                ResponsesStreamKernel::new(ResponsesDialectProfile::openai_platform(), limits)
                    .with_typed_terminal_cutoff();
            let mut hooks = TestHooks { fail_decode: false };
            let mut body = terminal.to_vec();
            body.extend_from_slice(suffix);
            let events = kernel
                .push(&body, &mut hooks)
                .unwrap_or_else(|error| panic!("push: {error:?}"));
            assert_eq!(events.len(), 1);
            assert!(events[0].terminal);
            assert_eq!(kernel.total_bytes(), terminal.len());
            assert!(kernel.finish(&mut hooks).is_ok());
        }
    }

    #[test]
    fn total_byte_limit_is_enforced() {
        let limits = ResponsesStreamLimits::new(32, 40, 16)
            .unwrap_or_else(|error| panic!("limits: {error}"));
        let mut kernel = ResponsesStreamKernel::new(ResponsesDialectProfile::xai(), limits);
        let mut hooks = TestHooks { fail_decode: false };
        let err = kernel
            .push(&[b'x'; 41], &mut hooks)
            .err()
            .unwrap_or_else(|| panic!("expected over-limit error"));
        assert_eq!(err, TestError::Limit(resource::TOTAL_STREAM_BYTES));
    }

    #[test]
    fn openai_auto_dispatch_residual_finish_decodes_unterminated_frame() {
        let mut kernel = ResponsesStreamKernel::new(
            ResponsesDialectProfile::openai_platform(),
            ResponsesStreamLimits::default(),
        );
        let mut hooks = TestHooks { fail_decode: false };
        // No trailing blank line: residual must be auto-dispatched at finish.
        let residual = b"data: {\"type\":\"done\",\"sequence_number\":0}\n";
        assert!(
            kernel
                .push(residual, &mut hooks)
                .unwrap_or_else(|error| panic!("{error:?}"))
                .is_empty()
        );
        let events = kernel
            .finish(&mut hooks)
            .unwrap_or_else(|error| panic!("finish residual: {error:?}"));
        assert_eq!(events.len(), 1);
        assert!(kernel.typed_terminal_seen());
    }

    #[test]
    fn xai_blank_line_finish_rejects_residual_frame() {
        let mut kernel = ResponsesStreamKernel::new(
            ResponsesDialectProfile::xai(),
            ResponsesStreamLimits::default(),
        );
        let mut hooks = TestHooks { fail_decode: false };
        let residual = b"data: {\"type\":\"done\",\"sequence_number\":0}\n";
        assert!(
            kernel
                .push(residual, &mut hooks)
                .unwrap_or_else(|error| panic!("{error:?}"))
                .is_empty()
        );
        assert!(matches!(
            kernel.finish(&mut hooks),
            Err(TestError::Truncated)
        ));
    }
}
