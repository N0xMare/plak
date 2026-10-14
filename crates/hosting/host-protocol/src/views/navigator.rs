//! Inert agent-tree observations that are not the tree's own rows: the
//! per-attempt facts a row carries and the status glyph table.
//!
//! The rows themselves are [`crate::views::tree::TreeView`], typed. The host
//! projects all of this; a driver only selects and emits typed intents. Stale revisions are the host's to resnapshot.

/// Per-attempt usage the host projects for one node.
///
/// Model-visible counters only: cost and quota balances never reach a row.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AttemptUsage {
    /// Wall time the attempt has been running, as the host measured it.
    pub elapsed_ms: u64,
    /// Admitted model turns on the attempt.
    pub turns: u32,
    /// Tool calls the attempt made.
    pub tool_calls: u32,
    /// Input tokens billed to the attempt.
    pub input_tokens: u64,
    /// Output tokens billed to the attempt.
    pub output_tokens: u64,
    /// Compaction observations derived from the retained child feed, never billing authority.
    pub compaction: Option<AttemptCompaction>,
}

/// Disposable per-attempt compaction projection; absent until the child feed is observed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct AttemptCompaction {
    /// Semantic replacements committed.
    pub committed: u64,
    /// Definite failed compactions.
    pub failed: u64,
    /// Passes without a replaceable range.
    pub no_eligible: u64,
    /// Measurements of the most recent committed compaction, not billed totals.
    pub tokens_before: Option<u64>,
    /// Measured successor input of that compaction.
    pub tokens_after: Option<u64>,
    /// False when feed retention prevents a complete reconstruction.
    pub complete: bool,
}

/// Why the current attempt is `blocked`.
///
/// A blocked attempt leaves the *node* `Failed`; the reset time lives on the
/// attempt, which is why the TUI reads it from here and not from the status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockedObservation {
    /// Provider-named quota that refused.
    pub quota: String,
    /// Account whose quota refused.
    pub account: String,
    /// Unix seconds the quota resets, when the provider named one.
    pub resets_at_unix: Option<u64>,
}

/// Painted for a label no projection produces. A label reaching this glyph is
/// a silent degradation, so [`PROJECTED_STATUS_LABELS`] pins the complete set
/// and the host asserts its own enums against it.
pub const UNKNOWN_STATUS_GLYPH: char = '\u{b7}';

/// Every status label the host projects onto a canvas row: node statuses,
/// the root's session-derived `idle`, workspace-row statuses,
/// and the attempt statuses the inspector paints. `status_glyph` has an arm
/// for each; a new host label with no arm here fails
/// `every_projected_status_label_has_its_own_glyph`.
pub const PROJECTED_STATUS_LABELS: &[&str] = &[
    // Node statuses.
    "pending",
    "running",
    "paused",
    "sealing",
    "completed",
    "interrupted",
    "failed",
    "closed",
    "recovery_required",
    // The root's status is the session's turn phase, not a durable node row.
    "idle",
    // Workspace-row statuses.
    "active",
    "adopting",
    "discarding",
    "clean_removed",
    "adopted",
    "discarded",
    // Attempt statuses not already spelled by a node status.
    "submitted",
    "completed_without_submission",
    "cancelled",
    "blocked",
];

/// Status glyph per spec 02 §5.7. Node boxes never animate, so `◐` is static.
///
/// Every arm is spelled out: the wildcard exists only to keep the function
/// total over `&str`, and reaching it is the bug
/// `every_projected_status_label_has_its_own_glyph` catches. `sealing`,
/// `adopting`, `adopted`, and `blocked` are the lifecycle observations, so
/// they carry glyphs of their own rather than sharing the fallback dot.
#[must_use]
pub fn status_glyph(status: &str) -> char {
    match status {
        "pending" => '\u{25cb}',
        "idle" => '\u{25cc}',
        "running" | "submitted" => '\u{25d0}',
        "paused" => '\u{25d1}',
        // Sealing is work in flight on the node itself.
        "sealing" => '\u{25cd}',
        // Adopting is work in flight on the candidate's workspace row.
        "adopting" => '\u{25ce}',
        "discarding" => '\u{25a1}',
        // A landed candidate: the filled cousin of `adopting`.
        "adopted" => '\u{25c9}',
        "completed" | "active" => '\u{25cf}',
        "completed_without_submission" => '\u{25d3}',
        "interrupted" | "closed" | "cancelled" | "clean_removed" | "discarded" => '\u{25a0}',
        // A quota refusal is not a crash; it is a barred road with a reset.
        "blocked" => '\u{2298}',
        "failed" | "recovery_required" => '\u{d7}',
        _ => UNKNOWN_STATUS_GLYPH,
    }
}
