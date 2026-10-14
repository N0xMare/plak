//! Dialect profile tables for the Responses-family wire kernel.
//!
//! These policies parameterize shared stream checks (sequence, terminal framing,
//! optional UTF-8 BOM). Commercial plugins supply a profile; the kernel does not
//! invent model-facing policy.

use thiserror::Error;

/// How a Responses-family dialect validates event sequence numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SequencePolicy {
    /// `sequence_number` is required on every event and must be contiguous from 0.
    RequiredStrict,
    /// Sequence fields may be omitted for the whole stream; when present they must
    /// be strictly monotonic with no gaps, and presence must not change mid-stream.
    ///
    /// `fields` names the wire keys accepted by the dialect, checked in order by
    /// extractors (first hit wins). Pure validators consume an already-extracted
    /// `Option<u64>` and do not parse JSON.
    OptionalMonotonic {
        /// Wire field names the dialect may use for sequence numbers.
        fields: &'static [&'static str],
    },
}

/// How a dialect accepts typed terminal events and optional SSE `[DONE]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TerminalPolicy {
    /// A typed terminal event is required; `[DONE]` is forbidden.
    TypedOnly,
    /// A typed terminal event must be followed by exactly one `[DONE]` marker.
    TypedThenDone,
    /// A typed terminal event is required; `[DONE]` after it is optional.
    TypedOptionalDone,
}

/// How EOF treats residual SSE fields that never received a blank-line dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FinishPolicy {
    /// Residual data/event fields or non-comment unfinished lines are truncated
    /// (xAI, `OpenRouter` Responses blank-line frame termination).
    BlankLineClosed,
    /// Residual unfinished frames are auto-dispatched like
    /// [`plakous_http::SseDecoder::finish`] (`OpenAI` Platform).
    AutoDispatchResidual,
}

/// How the first stream bytes treat a UTF-8 BOM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BomPolicy {
    /// Strip a leading UTF-8 BOM when present.
    Strip,
    /// Do not special-case a BOM (framing/UTF-8 layers reject or pass through).
    Ignore,
}

/// Composed Responses-family dialect defaults for one commercial plugin surface.
///
/// Profiles record **wire** conventions only. They do not own routes, auth, or
/// agent-loop policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesDialectProfile {
    sequence: SequencePolicy,
    terminal: TerminalPolicy,
    finish: FinishPolicy,
    bom: BomPolicy,
}

/// `OpenAI` Platform sequence field (strict required path).
pub const OPENAI_PLATFORM_SEQUENCE_FIELDS: &[&str] = &["sequence_number"];

/// xAI Platform / Grok subscription sequence field candidates.
pub const XAI_SEQUENCE_FIELDS: &[&str] = &["sequence_number", "sequence"];

/// `OpenRouter` stateless Responses beta sequence field.
pub const OPENROUTER_SEQUENCE_FIELDS: &[&str] = &["sequence_number"];

impl ResponsesDialectProfile {
    /// Defaults matching current `OpenAI` Platform Responses stream behavior.
    ///
    /// Sequence is required and contiguous from 0; typed terminal is required;
    /// `[DONE]` after a typed terminal is optional; residual frames at EOF are
    /// auto-dispatched; BOM is not stripped.
    #[must_use]
    pub const fn openai_platform() -> Self {
        Self {
            sequence: SequencePolicy::RequiredStrict,
            terminal: TerminalPolicy::TypedOptionalDone,
            finish: FinishPolicy::AutoDispatchResidual,
            bom: BomPolicy::Ignore,
        }
    }

    /// Defaults matching current xAI Responses stream behavior.
    ///
    /// Sequence is optional-monotonic over `sequence_number` / `sequence`; typed
    /// terminal only (`[DONE]` forbidden); blank-line frame termination; BOM is
    /// not stripped.
    #[must_use]
    pub const fn xai() -> Self {
        Self {
            sequence: SequencePolicy::OptionalMonotonic {
                fields: XAI_SEQUENCE_FIELDS,
            },
            terminal: TerminalPolicy::TypedOnly,
            finish: FinishPolicy::BlankLineClosed,
            bom: BomPolicy::Ignore,
        }
    }

    /// Defaults matching current `OpenRouter` stateless Responses beta behavior.
    ///
    /// Sequence is optional-monotonic over `sequence_number`; typed terminal must
    /// be followed by `[DONE]`; blank-line frame termination; leading UTF-8 BOM
    /// is stripped.
    #[must_use]
    pub const fn openrouter() -> Self {
        Self {
            sequence: SequencePolicy::OptionalMonotonic {
                fields: OPENROUTER_SEQUENCE_FIELDS,
            },
            terminal: TerminalPolicy::TypedThenDone,
            finish: FinishPolicy::BlankLineClosed,
            bom: BomPolicy::Strip,
        }
    }

    /// Returns the sequence validation policy.
    #[must_use]
    pub const fn sequence(self) -> SequencePolicy {
        self.sequence
    }

    /// Returns the terminal / `[DONE]` framing policy.
    #[must_use]
    pub const fn terminal(self) -> TerminalPolicy {
        self.terminal
    }

    /// Returns the residual-frame EOF policy.
    #[must_use]
    pub const fn finish(self) -> FinishPolicy {
        self.finish
    }

    /// Returns the leading-BOM policy.
    #[must_use]
    pub const fn bom(self) -> BomPolicy {
        self.bom
    }
}

/// Mutable sequence tracker for pure [`SequencePolicy`] checks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SequenceState {
    /// Last accepted sequence when the stream supplies numbers.
    last: Option<u64>,
    /// Whether the stream has established sequence presence (optional policy).
    presence: Option<bool>,
    /// Next required sequence under [`SequencePolicy::RequiredStrict`].
    expected: u64,
}

impl SequenceState {
    /// Creates an empty tracker (strict expected starts at 0).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: None,
            presence: None,
            expected: 0,
        }
    }

    /// Returns the last accepted sequence number, if any.
    #[must_use]
    pub const fn last(&self) -> Option<u64> {
        self.last
    }

    /// Returns established optional presence, if decided.
    #[must_use]
    pub const fn presence(&self) -> Option<bool> {
        self.presence
    }

    /// Returns the next expected sequence under strict policy.
    #[must_use]
    pub const fn expected(&self) -> u64 {
        self.expected
    }
}

/// Pure sequence-policy validation failure (no provider error types).
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SequenceValidationError {
    /// Required sequence was missing.
    #[error("required sequence number is missing")]
    MissingRequired,
    /// Optional sequence presence changed within one stream.
    #[error("sequence presence changed within one stream")]
    InconsistentPresence,
    /// Sequence repeated or moved backward.
    #[error("sequence {sequence} is not after {previous}")]
    DuplicateOrReordered {
        /// Last accepted sequence.
        previous: u64,
        /// Repeated or reordered sequence.
        sequence: u64,
    },
    /// A gap was observed under contiguous-sequence rules.
    #[error("sequence gap: expected {expected}, received {sequence}")]
    Gap {
        /// Required next sequence.
        expected: u64,
        /// Received sequence.
        sequence: u64,
    },
}

/// Validates one extracted sequence value under `policy`, updating `state`.
///
/// # Errors
///
/// Returns [`SequenceValidationError`] when the value violates the active policy.
pub fn validate_next_sequence(
    policy: SequencePolicy,
    state: &mut SequenceState,
    sequence: Option<u64>,
) -> Result<(), SequenceValidationError> {
    match policy {
        SequencePolicy::RequiredStrict => {
            let Some(sequence) = sequence else {
                return Err(SequenceValidationError::MissingRequired);
            };
            if sequence != state.expected {
                if let Some(previous) = state.last
                    && sequence <= previous
                {
                    return Err(SequenceValidationError::DuplicateOrReordered {
                        previous,
                        sequence,
                    });
                }
                return Err(SequenceValidationError::Gap {
                    expected: state.expected,
                    sequence,
                });
            }
            state.last = Some(sequence);
            state.presence = Some(true);
            state.expected = state.expected.saturating_add(1);
            Ok(())
        }
        SequencePolicy::OptionalMonotonic { fields: _ } => {
            let present = sequence.is_some();
            if state.presence.is_some_and(|expected| expected != present) {
                return Err(SequenceValidationError::InconsistentPresence);
            }
            state.presence = Some(present);
            let Some(sequence) = sequence else {
                return Ok(());
            };
            if let Some(previous) = state.last {
                if sequence <= previous {
                    return Err(SequenceValidationError::DuplicateOrReordered {
                        previous,
                        sequence,
                    });
                }
                let expected = previous.saturating_add(1);
                if sequence != expected {
                    return Err(SequenceValidationError::Gap { expected, sequence });
                }
            }
            state.last = Some(sequence);
            Ok(())
        }
    }
}

/// Classification of an SSE `[DONE]` data payload under a [`TerminalPolicy`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DoneMarkerDecision {
    /// Accept the marker (optionally record that DONE was seen).
    Accept,
    /// The dialect forbids `[DONE]` entirely.
    Forbidden,
    /// Marker arrived before a typed terminal, or duplicated when only one is allowed.
    OutOfOrder,
}

/// Decides how to treat a `[DONE]` marker given observed terminal state.
#[must_use]
pub const fn decide_done_marker(
    policy: TerminalPolicy,
    typed_terminal_seen: bool,
    done_marker_seen: bool,
) -> DoneMarkerDecision {
    match policy {
        TerminalPolicy::TypedOnly => DoneMarkerDecision::Forbidden,
        TerminalPolicy::TypedThenDone => {
            if typed_terminal_seen && !done_marker_seen {
                DoneMarkerDecision::Accept
            } else {
                DoneMarkerDecision::OutOfOrder
            }
        }
        TerminalPolicy::TypedOptionalDone => {
            if typed_terminal_seen {
                // OpenAI Platform ignores extra `[DONE]` after a typed terminal.
                DoneMarkerDecision::Accept
            } else {
                DoneMarkerDecision::OutOfOrder
            }
        }
    }
}

/// Reports whether EOF satisfies the dialect terminal framing.
#[must_use]
pub const fn terminal_finish_ok(
    policy: TerminalPolicy,
    typed_terminal_seen: bool,
    done_marker_seen: bool,
) -> bool {
    match policy {
        TerminalPolicy::TypedOnly | TerminalPolicy::TypedOptionalDone => typed_terminal_seen,
        TerminalPolicy::TypedThenDone => typed_terminal_seen && done_marker_seen,
    }
}

/// Reports whether the dialect requires a trailing `[DONE]` after a typed terminal.
#[must_use]
pub const fn requires_done_marker(policy: TerminalPolicy) -> bool {
    matches!(policy, TerminalPolicy::TypedThenDone)
}

/// Reports whether the dialect strips a leading UTF-8 BOM.
#[must_use]
pub const fn strips_leading_bom(policy: BomPolicy) -> bool {
    matches!(policy, BomPolicy::Strip)
}

/// UTF-8 byte-order mark recognized when [`BomPolicy::Strip`] is active.
pub const UTF8_BOM: &[u8; 3] = b"\xef\xbb\xbf";

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    #[test]
    fn openai_profile_matches_platform_defaults() {
        let profile = ResponsesDialectProfile::openai_platform();
        assert_eq!(profile.sequence(), SequencePolicy::RequiredStrict);
        assert_eq!(profile.terminal(), TerminalPolicy::TypedOptionalDone);
        assert_eq!(profile.finish(), FinishPolicy::AutoDispatchResidual);
        assert_eq!(profile.bom(), BomPolicy::Ignore);
        assert!(!strips_leading_bom(profile.bom()));
        assert!(!requires_done_marker(profile.terminal()));
    }

    #[test]
    fn xai_profile_matches_typed_only_defaults() {
        let profile = ResponsesDialectProfile::xai();
        assert_eq!(
            profile.sequence(),
            SequencePolicy::OptionalMonotonic {
                fields: XAI_SEQUENCE_FIELDS,
            }
        );
        assert_eq!(profile.terminal(), TerminalPolicy::TypedOnly);
        assert_eq!(profile.finish(), FinishPolicy::BlankLineClosed);
        assert_eq!(profile.bom(), BomPolicy::Ignore);
        assert_eq!(
            decide_done_marker(profile.terminal(), true, false),
            DoneMarkerDecision::Forbidden
        );
        assert!(terminal_finish_ok(profile.terminal(), true, false));
        assert!(!terminal_finish_ok(profile.terminal(), false, false));
    }

    #[test]
    fn openrouter_profile_requires_done_and_strips_bom() {
        let profile = ResponsesDialectProfile::openrouter();
        assert_eq!(
            profile.sequence(),
            SequencePolicy::OptionalMonotonic {
                fields: OPENROUTER_SEQUENCE_FIELDS,
            }
        );
        assert_eq!(profile.terminal(), TerminalPolicy::TypedThenDone);
        assert_eq!(profile.finish(), FinishPolicy::BlankLineClosed);
        assert_eq!(profile.bom(), BomPolicy::Strip);
        assert!(requires_done_marker(profile.terminal()));
        assert!(strips_leading_bom(profile.bom()));
        assert_eq!(
            decide_done_marker(profile.terminal(), true, false),
            DoneMarkerDecision::Accept
        );
        assert_eq!(
            decide_done_marker(profile.terminal(), true, true),
            DoneMarkerDecision::OutOfOrder
        );
        assert_eq!(
            decide_done_marker(profile.terminal(), false, false),
            DoneMarkerDecision::OutOfOrder
        );
        assert!(!terminal_finish_ok(profile.terminal(), true, false));
        assert!(terminal_finish_ok(profile.terminal(), true, true));
    }

    #[test]
    fn required_strict_accepts_contiguous_from_zero() {
        let mut state = SequenceState::new();
        let policy = SequencePolicy::RequiredStrict;
        validate_next_sequence(policy, &mut state, Some(0)).expect("0");
        validate_next_sequence(policy, &mut state, Some(1)).expect("1");
        validate_next_sequence(policy, &mut state, Some(2)).expect("2");
        assert_eq!(state.expected(), 3);
        assert_eq!(state.last(), Some(2));
    }

    #[test]
    fn required_strict_rejects_missing_gap_and_reorder() {
        let mut state = SequenceState::new();
        let policy = SequencePolicy::RequiredStrict;
        assert_eq!(
            validate_next_sequence(policy, &mut state, None),
            Err(SequenceValidationError::MissingRequired)
        );
        validate_next_sequence(policy, &mut state, Some(0)).expect("0");
        assert_eq!(
            validate_next_sequence(policy, &mut state, Some(2)),
            Err(SequenceValidationError::Gap {
                expected: 1,
                sequence: 2,
            })
        );
        assert_eq!(
            validate_next_sequence(policy, &mut state, Some(0)),
            Err(SequenceValidationError::DuplicateOrReordered {
                previous: 0,
                sequence: 0,
            })
        );
    }

    #[test]
    fn optional_monotonic_allows_absent_or_contiguous() {
        let policy = SequencePolicy::OptionalMonotonic {
            fields: OPENROUTER_SEQUENCE_FIELDS,
        };
        let mut absent = SequenceState::new();
        validate_next_sequence(policy, &mut absent, None).expect("absent");
        validate_next_sequence(policy, &mut absent, None).expect("still absent");
        assert_eq!(absent.presence(), Some(false));

        let mut present = SequenceState::new();
        validate_next_sequence(policy, &mut present, Some(1)).expect("first any");
        validate_next_sequence(policy, &mut present, Some(2)).expect("next");
        assert_eq!(
            validate_next_sequence(policy, &mut present, Some(4)),
            Err(SequenceValidationError::Gap {
                expected: 3,
                sequence: 4,
            })
        );
        assert_eq!(
            validate_next_sequence(policy, &mut present, Some(2)),
            Err(SequenceValidationError::DuplicateOrReordered {
                previous: 2,
                sequence: 2,
            })
        );
    }

    #[test]
    fn optional_monotonic_rejects_presence_flip() {
        let policy = SequencePolicy::OptionalMonotonic {
            fields: XAI_SEQUENCE_FIELDS,
        };
        let mut state = SequenceState::new();
        validate_next_sequence(policy, &mut state, Some(0)).expect("present");
        assert_eq!(
            validate_next_sequence(policy, &mut state, None),
            Err(SequenceValidationError::InconsistentPresence)
        );

        let mut other = SequenceState::new();
        validate_next_sequence(policy, &mut other, None).expect("absent");
        assert_eq!(
            validate_next_sequence(policy, &mut other, Some(1)),
            Err(SequenceValidationError::InconsistentPresence)
        );
    }

    #[test]
    fn typed_optional_done_accepts_after_terminal_only() {
        let policy = TerminalPolicy::TypedOptionalDone;
        assert_eq!(
            decide_done_marker(policy, false, false),
            DoneMarkerDecision::OutOfOrder
        );
        assert_eq!(
            decide_done_marker(policy, true, false),
            DoneMarkerDecision::Accept
        );
        assert_eq!(
            decide_done_marker(policy, true, true),
            DoneMarkerDecision::Accept
        );
        assert!(terminal_finish_ok(policy, true, false));
        assert!(terminal_finish_ok(policy, true, true));
        assert!(!terminal_finish_ok(policy, false, true));
    }
}
