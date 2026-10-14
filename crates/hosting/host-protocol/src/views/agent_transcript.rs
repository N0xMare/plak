//! One agent's transcript for the surface's main view (spec 03 §16).

use super::transcript::TranscriptRow;

/// The committed history of one agent of the attached session's tree, as the
/// same typed rows the root transcript shows. Answers
/// [`SessionIntent::OpenAgentTranscript`](crate::intent::SessionIntent::OpenAgentTranscript);
/// a later one for the same agent replaces it. In-process only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTranscript {
    /// Exact agent id from the tree.
    pub agent_id: String,
    /// Committed canonical history as typed rows, oldest first, within
    /// `MAX_SNAPSHOT_ROWS` / `MAX_PRESENTATION_BYTES`; `Truncated` is the first
    /// row when older rows were dropped.
    pub rows: Vec<TranscriptRow>,
    /// The live turn whose `ChildLive` ops follow, when one is attached.
    pub live_turn_id: Option<String>,
    /// Fail-closed note: no session yet, unreadable history, not in tree.
    pub note: Option<String>,
}
