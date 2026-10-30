//! Atomic attempt lifecycle state machine shared by Responses-family dialects.
//!
//! Dialects own observation ledgers and public error enums; this module only
//! fences the pure `AtomicU8` phase transitions used by xAI and OpenRouter.

use std::sync::atomic::{AtomicU8, Ordering};

/// Prepared, not yet admitted for decode or dispatch observation.
pub const PREPARED: u8 = 0;
/// Logically admitted; ready for physical requests / decode begin.
pub const ADMITTED: u8 = 1;
/// Decode in progress (complete body or stream push).
pub const DECODING: u8 = 2;
/// Decode completed successfully; ready for terminal success seal.
pub const DECODED: u8 = 3;
/// Decode failed while in [`DECODING`]; may still claim protocol failure.
pub const DECODE_FAILED: u8 = 4;
/// Failed / sealed without successful terminal completion.
pub const FAILED: u8 = 5;
/// Terminal success sealed after [`DECODED`].
pub const TERMINAL: u8 = 6;

/// Maximum pending service polls used to join cleanup after cancellation.
///
/// Dialects re-export this as `MAX_*_ADMITTED_CLEANUP_POLLS`.
pub const MAX_ADMITTED_CLEANUP_POLLS: usize = 64;

/// Failure of a pure lifecycle transition (not admission observation errors).
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum AttemptLifecycleError {
    /// The atomic phase was not in the expected pre-state for the transition.
    #[error("attempt lifecycle transition is invalid for the current terminal state")]
    InvalidTerminalState,
}

/// Reports whether the attempt is still active for provider/protocol work.
#[must_use]
pub fn is_active(lifecycle: &AtomicU8) -> bool {
    matches!(lifecycle.load(Ordering::Acquire), ADMITTED | DECODING)
}

/// Reports whether the attempt is currently decoding a response.
#[must_use]
pub fn is_decoding(lifecycle: &AtomicU8) -> bool {
    lifecycle.load(Ordering::Acquire) == DECODING
}

/// Transitions `ADMITTED → DECODING`.
///
/// # Errors
///
/// Returns [`AttemptLifecycleError::InvalidTerminalState`] when not admitted.
pub fn begin_decoding(lifecycle: &AtomicU8) -> Result<(), AttemptLifecycleError> {
    lifecycle
        .compare_exchange(ADMITTED, DECODING, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| AttemptLifecycleError::InvalidTerminalState)
}

/// Transitions `DECODING → DECODED`.
///
/// # Errors
///
/// Returns [`AttemptLifecycleError::InvalidTerminalState`] when not decoding.
pub fn mark_decoded(lifecycle: &AtomicU8) -> Result<(), AttemptLifecycleError> {
    lifecycle
        .compare_exchange(DECODING, DECODED, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| AttemptLifecycleError::InvalidTerminalState)
}

/// Transitions `DECODING → ADMITTED` after an interrupted stream the logical
/// attempt will replay in a new physical request.
///
/// # Errors
///
/// Returns [`AttemptLifecycleError::InvalidTerminalState`] when not decoding.
pub fn resume_after_interrupted_stream(lifecycle: &AtomicU8) -> Result<(), AttemptLifecycleError> {
    lifecycle
        .compare_exchange(DECODING, ADMITTED, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| AttemptLifecycleError::InvalidTerminalState)
}

/// Best-effort `DECODING → DECODE_FAILED` after a decode error.
pub fn mark_decode_failed(lifecycle: &AtomicU8) {
    let _ =
        lifecycle.compare_exchange(DECODING, DECODE_FAILED, Ordering::AcqRel, Ordering::Acquire);
}

/// Claims protocol-failure ownership: active or decode-failed → [`FAILED`].
///
/// Returns `true` when this caller wins the claim.
#[must_use]
pub fn claim_protocol_failure(lifecycle: &AtomicU8) -> bool {
    loop {
        let current = lifecycle.load(Ordering::Acquire);
        if !matches!(current, ADMITTED | DECODING | DECODE_FAILED) {
            return false;
        }
        if lifecycle
            .compare_exchange(current, FAILED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }
    }
}

/// Seals a service attempt into [`FAILED`] unless already failed or terminal.
///
/// Dialects close observation ledgers separately; this is the pure phase seal.
pub fn seal_service_attempt(lifecycle: &AtomicU8) {
    loop {
        let current = lifecycle.load(Ordering::Acquire);
        if matches!(current, FAILED | TERMINAL) {
            return;
        }
        if lifecycle
            .compare_exchange(current, FAILED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
