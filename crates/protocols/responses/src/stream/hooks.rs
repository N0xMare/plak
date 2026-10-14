//! Dialect hooks for the Responses-family stream kernel (front-half).
//!
//! The kernel owns framing, BOM, byte/event limits, sequence tracking, and
//! `[DONE]` / terminal finish checks. Shared item lifecycle lives in
//! [`super::lifecycle::ResponsesLifecycle`]. Dialects own JSON event-shape
//! parsing and call into the lifecycle from
//! [`DialectStreamHooks::decode_json_event`].

use plakous_http::SseError;

use crate::dialect::SequenceValidationError;
use crate::json_fields::ResponsesFieldError;

use super::limits::ResponsesStreamLimits;

/// Dialect-owned callbacks for one Responses-family stream decode session.
///
/// # Front-half contract
///
/// - Kernel frames SSE, strips BOM when profiled, enforces stream limits,
///   validates sequence numbers, and applies terminal/`[DONE]` policy.
/// - Dialect maps kernel failures into its error enum and decodes JSON events
///   (including provider lifecycle rules) in [`Self::decode_json_event`].
pub trait DialectStreamHooks {
    /// One successfully decoded dialect event.
    type Event;
    /// Dialect stream error type (provider-specific).
    type Error;

    /// Maps a shared SSE framing error using the active stream limits.
    fn map_sse_error(err: SseError, limits: &ResponsesStreamLimits) -> Self::Error;

    /// Maps a pure sequence-policy validation failure.
    fn map_sequence_error(err: SequenceValidationError) -> Self::Error;

    /// Maps a forbidden or otherwise rejected `[DONE]` marker.
    fn map_done_forbidden() -> Self::Error;

    /// Maps JSON or framing content after the dialect stream is closed.
    fn map_event_after_terminal() -> Self::Error;

    /// Maps EOF inside an unfinished SSE frame.
    fn map_truncated() -> Self::Error;

    /// Maps EOF without a required typed terminal event.
    fn map_missing_terminal() -> Self::Error;

    /// Maps EOF without a required trailing `[DONE]` marker.
    fn map_missing_done() -> Self::Error;

    /// Maps a stream-level resource limit (total bytes or event count).
    fn map_resource_limit(resource: &'static str, maximum: usize) -> Self::Error;

    /// Maps a fail-closed JSON field error raised while extracting sequence.
    fn map_field_error(err: ResponsesFieldError) -> Self::Error;

    /// Reports whether `event` is a typed terminal condition for this dialect.
    fn event_is_terminal(event: &Self::Event) -> bool;

    /// Decodes one non-`[DONE]` JSON SSE data payload.
    ///
    /// `sequence` is the value already extracted and validated by the kernel
    /// under the active [`crate::ResponsesDialectProfile`] sequence policy.
    /// Return `Ok(None)` for frames that intentionally yield no dialect event.
    ///
    /// # Errors
    ///
    /// Returns a dialect error for malformed JSON, invalid documented shapes,
    /// lifecycle violations, or other fail-closed plugin rules.
    fn decode_json_event(
        &mut self,
        data: &str,
        event_name: Option<&str>,
        sequence: Option<u64>,
    ) -> Result<Option<Self::Event>, Self::Error>;
}
