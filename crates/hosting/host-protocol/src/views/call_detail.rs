//! The answer to one `RequestCallDetail` intent: a tool call's full input and
//! result, read on demand so the transcript can expand any item.

use std::fmt;

/// Where a call detail's text came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallDetailSource {
    /// The session's committed canonical history: the input and the result
    /// exactly as the model saw them, redacted and bounded per side.
    Canonical,
    /// The attached session's retained transcript tool row (subject and
    /// bounded detail), because the call is no longer in its canonical history
    /// (compacted, pruned, not carried) or not committed yet. Never the whole
    /// call, and never for a child.
    Preview,
}

/// What the host could read for one call.
#[derive(Clone, Eq, PartialEq)]
pub enum CallDetailOutcome {
    /// Displayable call text.
    Detail {
        /// The call's arguments (pretty JSON when they parse).
        input: String,
        /// The result text, empty when the source holds none.
        output: String,
        /// Whether `input` is incomplete: clipped at the bound, or a preview
        /// (which never holds the whole call).
        input_truncated: bool,
        /// Whether `output` is incomplete, as for `input_truncated`.
        output_truncated: bool,
        /// Which store answered.
        source: CallDetailSource,
    },
    /// The call is not part of the addressed history, the addressed agent is
    /// not a child of the attached tree, or nothing is readable for it.
    Unavailable {
        /// Bounded operator-facing reason.
        reason: String,
    },
}

impl fmt::Debug for CallDetailOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Detail {
                input,
                output,
                input_truncated,
                output_truncated,
                source,
            } => formatter
                .debug_struct("Detail")
                .field("input_bytes", &input.len())
                .field("output_bytes", &output.len())
                .field("input_truncated", input_truncated)
                .field("output_truncated", output_truncated)
                .field("source", source)
                .finish(),
            Self::Unavailable { reason } => formatter
                .debug_struct("Unavailable")
                .field("reason", reason)
                .finish(),
        }
    }
}

/// The host's answer to one call-detail request, keyed by the requested id
/// and the session the host resolved against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallDetail {
    /// The exact call id the request named.
    pub call_id: String,
    /// The child agent id the request named; `None` for the attached session.
    pub agent_id: Option<String>,
    /// The attached product session the host resolved this read against.
    /// The surface drops an answer whose id is not the session it has attached.
    pub session_id: String,
    /// What was readable.
    pub outcome: CallDetailOutcome,
}
