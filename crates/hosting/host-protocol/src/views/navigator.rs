//! Inert agent-tree observations that are not the tree's own rows: the
//! per-attempt facts and neutral projected status labels.
//!
//! The rows themselves are [`crate::views::tree::TreeView`], typed. The host
//! projects all of this; a driver only selects and emits typed intents. Stale revisions are the host's to resnapshot.

/// Per-attempt usage the host projects for one node.
///
/// Model-visible counters only: cost and quota balances never reach a row.
#[derive(
    serde::Serialize,
    serde::Deserialize,
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
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
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
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
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BlockedObservation {
    /// Provider-named quota that refused.
    pub quota: String,
    /// Account whose quota refused.
    pub account: String,
    /// Unix seconds the quota resets, when the provider named one.
    pub resets_at_unix: Option<u64>,
}

/// Every status label the host projects onto a canvas row: node statuses,
/// the root's session-derived `idle`, workspace-row statuses,
/// and the attempt statuses the inspector presents. Frontends own their glyphs;
/// the TUI checks its table against this complete set.
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
