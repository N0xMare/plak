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

    /// Returns the sequence tracker.
    #[must_use]
    pub const fn sequence(&self) -> &SequenceState {
        &self.sequence
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
mod tests;
