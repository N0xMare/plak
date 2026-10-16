//! Typed, bounded presentation data shared by the Plak host and TUI.
//!
//! This is an observational read model. It is safe to drop, coalesce, replace,
//! or rebuild from authoritative product/runtime state. A
//! [`PresentationSnapshot`] is never terminal-result, command, checkpoint, or
//! recovery authority.

#![allow(
    missing_docs,
    reason = "internal cross-package presentation DTO; module-level authority and invariants are documented"
)]

use std::borrow::Cow;

use super::transcript::TranscriptRow;

/// Shared current-activity vocabulary for the root HUD and live child rows.
///
/// `name` is a canonical tool identity, never an exposed spelling: `wait`,
/// `wait_js` and `wait_py` here are the composition cell-wait markers, and
/// every process await (pi's `wait` included) arrives as `process_session_poll`.
///
/// A call to a tool exposed as `mcp__<server>__<tool>` reads
/// `calling mcp:<server>`.
#[must_use]
pub fn activity_label(name: &str, kind: ActivityKind) -> Cow<'static, str> {
    if kind == ActivityKind::Tool
        && let Some(call) = McpCall::parse(name)
    {
        return Cow::Owned(format!("calling mcp:{}", call.server));
    }
    Cow::Borrowed(builtin_activity_label(name, kind))
}

fn builtin_activity_label(name: &str, kind: ActivityKind) -> &'static str {
    if matches!(
        name,
        "wait" | "wait_js" | "wait_py" | "process_session_poll" | "write_stdin"
    ) {
        return "waiting";
    }
    if name == "wait_agent" {
        return "waiting on agents";
    }
    if matches!(
        name,
        "spawn_agent" | "list_agents" | "send_agent_message" | "adopt_candidate"
    ) {
        return "working";
    }
    match kind {
        ActivityKind::Agent => return "waiting on agents",
        ActivityKind::Compact => return "compacting",
        ActivityKind::Compose => return "working",
        _ => {}
    }
    match name {
        "run_process"
        | "exec_command"
        | "run_terminal_cmd"
        | "run_terminal_command"
        | "Shell"
        | "shell"
        | "bash"
        | "Bash"
        | "process_session_start"
        | "process_session_write"
        | "process_session_terminate" => "running command",
        "read_file" | "view_file" | "view_image" | "inspect_image" | "Read" | "web_fetch"
        | "WebFetch" | "fetch" | "fetch_url" | "web__run" | "open_url" | "list_dir"
        | "list_directory" | "ls" => "reading",
        "search_repository" | "grep" | "Grep" | "glob" | "Glob" | "codebase_search"
        | "file_search" | "web_search" | "WebSearch" | "x_search" => "searching",
        "apply_patch" | "search_replace" | "edit_file" | "write_file" | "create_file" | "Edit"
        | "Write" | "write" => "editing",
        _ => "working",
    }
}

/// A call presented as an MCP tool: the server and tool parts of an
/// exposed `mcp__<server>__<tool>` name.
///
/// The server-id grammar admits no `__`, so the split is unambiguous. This is
/// presentation only; execution ownership stays with the canonical call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McpCall<'a> {
    pub server: &'a str,
    pub tool: &'a str,
}

impl<'a> McpCall<'a> {
    /// Splits an exposed MCP tool name; `None` for any other name.
    #[must_use]
    pub fn parse(name: &'a str) -> Option<Self> {
        let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
        let server_valid = server
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
            && server
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        (server_valid && !tool.is_empty()).then_some(Self { server, tool })
    }
}

/// Schema of the typed-row restore snapshot.
pub const PRESENTATION_SCHEMA_V2: u32 = 2;
/// Maximum rows one snapshot restores.
pub const MAX_SNAPSHOT_ROWS: usize = 16_384;
/// Maximum approximate string bytes in a snapshot, with per-row headroom above
/// Plak's 16 MiB transcript payload.
pub const MAX_PRESENTATION_BYTES: usize = 18 * 1024 * 1024;

/// A complete replaceable transcript presentation: the typed rows a restore
/// renders, oldest first. The same rows the host emits live.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationSnapshot {
    /// Wire/read-model schema, independent of semantic runtime revisions.
    pub schema: u32,
    /// Monotonic position within this replaceable presentation epoch.
    pub revision: u64,
    /// Ordered transcript rows.
    pub rows: Vec<TranscriptRow>,
}

impl PresentationSnapshot {
    /// An empty snapshot at the supplied local revision.
    #[must_use]
    pub const fn empty(revision: u64) -> Self {
        Self {
            schema: PRESENTATION_SCHEMA_V2,
            revision,
            rows: Vec::new(),
        }
    }

    /// Checks the schema and the row and byte bounds.
    ///
    /// # Errors
    ///
    /// Returns the first violated presentation invariant.
    pub fn validate(&self) -> Result<(), PresentationError> {
        if self.schema != PRESENTATION_SCHEMA_V2 {
            return Err(PresentationError::UnsupportedSchema(self.schema));
        }
        if self.rows.len() > MAX_SNAPSHOT_ROWS {
            return Err(PresentationError::ItemLimit);
        }
        if self
            .rows
            .iter()
            .map(row_bytes)
            .fold(0_usize, usize::saturating_add)
            > MAX_PRESENTATION_BYTES
        {
            return Err(PresentationError::ByteLimit);
        }
        Ok(())
    }
}

impl Default for PresentationSnapshot {
    fn default() -> Self {
        Self::empty(0)
    }
}

/// Approximate retained bytes of one row: its strings plus fixed overhead.
#[must_use]
pub fn row_bytes(row: &TranscriptRow) -> usize {
    serde_json::to_string(row).map_or(usize::MAX, |json| json.len())
}

/// Presentation snapshot failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentationError {
    UnsupportedSchema(u32),
    ItemLimit,
    ByteLimit,
}

/// One collapsed Tact-like activity row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityItem {
    /// Stable product/call identity, never a display label.
    pub id: String,
    /// Optional cross-projection identity used only to merge observations.
    pub correlation_id: Option<String>,
    /// Host-supplied outer composition marker call owning this row.
    pub parent_call_id: Option<String>,
    /// Host-supplied local-composition cell owning this row.
    pub cell_id: Option<String>,
    pub kind: ActivityKind,
    pub label: String,
    pub subject: Option<String>,
    pub detail: Option<String>,
    pub state: ActivityState,
    /// Display timestamp in Unix milliseconds, when known.
    pub started_at_ms: Option<u64>,
    /// Display timestamp in Unix milliseconds, when known.
    pub finished_at_ms: Option<u64>,
    /// Observed elapsed time; it need not be derived from wall-clock fields.
    pub duration_ms: Option<u64>,
}

impl ActivityItem {
    /// Fills elapsed time from ordered display timestamps when it is absent.
    #[must_use]
    pub fn with_derived_duration(mut self) -> Self {
        if self.duration_ms.is_none() {
            self.duration_ms = self
                .started_at_ms
                .zip(self.finished_at_ms)
                .and_then(|(start, finish)| finish.checked_sub(start));
        }
        self
    }
}

/// Product-visible activity category, separate from model-facing tool names.
///
/// The host is the sole authority for this value. The renderer
/// never re-derives it from a display label, a tool name, or a detail body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// An ordinary tool call.
    #[default]
    Tool,
    /// A shell or process-start activity, normalized by the host.
    Process,
    /// A model-side observation rather than a tool call.
    Model,
    /// A topology tool acting on the agent tree.
    Agent,
    /// A compaction workflow: one row, in flight then sealed, never a grey note.
    Compact,
    /// One local-composition cell, owning the nested calls of that cell.
    ///
    /// A Compose row's counts are *host-counted over a droppable observation
    /// stream*: they state what the host observed, never what the cell
    /// executed, and they are never a billing or capacity claim.
    Compose,
}

/// Honest activity lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityState {
    Pending,
    Running,
    Succeeded,
    Failed { reason: Option<String> },
    Cancelled,
}

/// Context and terminal usage remain observations with their qualifications.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageView {
    pub context: Option<ContextUsage>,
    pub turn: Option<TurnUsage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextUsage {
    /// Host-owned categorical breakdown backing the Context section's
    /// 100-cell grid. `None` when the projection carries no breakdown; the
    /// renderer then shows one `Used` category rather than inventing shares.
    pub breakdown: Option<ContextBreakdownView>,
    pub prepared_input: Option<PreparedInputUsage>,
    /// The model's total context window: the composer-meter and Context-header
    /// denominator, and the base of the capacity-derived auto-compact fence.
    pub total_window: Option<u64>,
    /// The admitted input ceiling (dispatch gate). Shown as its own labelled
    /// fact when it sits under the window; never the meter denominator.
    pub admitted_ceiling: Option<AdmittedContextCeiling>,
    pub gauge_percent: Option<u8>,
    pub display_percent: Option<u128>,
    pub ceiling_excess: Option<ContextCeilingExcess>,
    pub evidence: String,
    pub non_guarantee: Option<String>,
    pub status: String,
}

/// Categorical context breakdown for the Usage Context section (spec 02 §5.6).
///
/// Everything here is host-projected from the Plakous context projection. A
/// value the projection does not carry is absent, never a zero: the renderer
/// prints `not reported` instead of a fabricated share.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextBreakdownView {
    /// Occupying tokens for the chrome meter and Context board: the Plakous
    /// context measure (last provider `input_tokens` of a sent request plus
    /// `bytes÷4` of what was appended since, else `bytes÷4` of the request).
    pub used_tokens: u64,
    /// Total admitted window.
    pub total_tokens: u64,
    /// Occupying categories in bar fill order (system → tools → auto-loaded
    /// → messages → reasoning → envelope JSON as `Used (unattributed)`). When
    /// occupancy exceeds their `bytes÷4` sum, that excess is its own
    /// `Provider-observed (est.)` slice; a smaller occupancy scales them
    /// down. A remainder between their sum and `used_tokens` still renders as
    /// unattributed rather than being dropped.
    pub categories: Vec<ContextCategoryView>,
    /// Informational rows that report their own cost and never enter the bar.
    pub informational: Vec<ContextInfoRowView>,
    /// The exact capacity-derived token trigger, when compaction is enabled.
    pub auto_compact_trigger_tokens: Option<u64>,
    /// Whether a provider count anchors `used_tokens`. The occupancy fences
    /// act only on an anchored measure (spec 01 §10.3).
    pub occupancy_anchored: bool,
    /// Admitted root turns, when the host counts them.
    pub turns: Option<u64>,
    /// Tool calls observed, when the host counts them.
    pub tool_calls: Option<u64>,
    /// Compactions observed, when the host counts them.
    pub compactions: Option<u64>,
}

/// One occupying category on the Context board.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCategoryView {
    /// Operator-facing label, e.g. `System`.
    pub label: String,
    /// Tokens this category occupies.
    pub tokens: u64,
}

/// One `◈` informational row. It reports its own token cost and is never a
/// bar category (spec 02 §5.6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextInfoRowView {
    /// Operator-facing label, e.g. `Tool definitions`.
    pub label: String,
    /// Tokens this row costs.
    pub tokens: u64,
    /// Trailing detail, e.g. `12 tools`.
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCeilingExcess {
    ExactOverflow(u64),
    UpperBoundExcess(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedInputUsage {
    Exact(u64),
    UpperBound(u64),
}

impl PreparedInputUsage {
    #[must_use]
    pub const fn tokens(self) -> u64 {
        match self {
            Self::Exact(tokens) | Self::UpperBound(tokens) => tokens,
        }
    }

    #[must_use]
    pub const fn is_upper_bound(self) -> bool {
        matches!(self, Self::UpperBound(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedContextCeiling {
    pub tokens: u64,
    pub basis: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cached_input_tokens: u64,
    /// Provider-reported cache-write tokens summed across reporting operations.
    pub cache_write_tokens: u64,
    /// `None` on a live cumulative snapshot whose coverage is not yet known.
    pub model_operations: Option<u64>,
    /// `None` on a live cumulative snapshot whose coverage is not yet known.
    pub reporting_model_operations: Option<u64>,
}

/// Disposition of the host's subscription-allowance read.
///
/// The Limits section is rendered from [`UsageLimitsView`], not from this: the
/// only thing the tab needs here is whether a read is in flight, has failed
/// with a reason, or has landed. It carried a full provider/meter/window
/// payload that every consumer discarded, rebuilt on a worker thread at every
/// refresh; the payload is gone.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SubscriptionUsageView {
    /// The product host has begun a read but has not supplied a terminal observation.
    #[default]
    Loading,
    /// No snapshot can truthfully be shown.
    Unavailable {
        /// Operator-facing, redaction-safe reason supplied by the product host.
        reason: String,
    },
    /// A terminal observation landed; its numbers are in [`UsageLimitsView`].
    Read,
}

/// One Usage "Limits" vertical, as the host names it.
///
/// The roster is host data, not protocol vocabulary: the product supplies the
/// account identifier it logs in with and the operator-facing label for it,
/// and the protocol only carries the pair. A host with three verticals, or
/// twelve, needs no change here.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UsageVertical {
    account: String,
    label: String,
}

impl UsageVertical {
    /// Builds one vertical from the host's account id and its label.
    #[must_use]
    pub fn new(account: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            label: label.into(),
        }
    }

    /// The host's account identifier for this vertical.
    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }

    /// The operator-facing label the host supplied for this vertical.
    #[must_use]
    pub fn provider_label(&self) -> &str {
        &self.label
    }
}

/// One complete host-owned Usage projection for the Limits section.
///
/// `verticals` always carries every vertical of the host's roster, in the
/// host's order: a vertical with no data is listed with its reason, never
/// omitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageLimitsView {
    /// Every vertical of the host's roster, in the host's order.
    pub verticals: Vec<VerticalUsageView>,
    /// Session token/call ledger since session start or last resume.
    pub session: SessionLedgerView,
    /// Age of this projection in whole seconds when it is being served stale
    /// (an in-turn `/usage`). `None` means the projection is current.
    pub stale_age_seconds: Option<u64>,
}

/// One vertical's allowance block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerticalUsageView {
    /// Which vertical this block describes.
    pub vertical: UsageVertical,
    /// Authenticated account identity exactly as the endpoint reported it,
    /// rendered as `Account: <email> (<plan>)`. `None` omits the line.
    pub account_email: Option<String>,
    /// Plan or tier exactly as the authenticated endpoint reported it. `None`
    /// omits the parenthetical; it is never guessed.
    pub plan_label: Option<String>,
    /// Redaction-safe observation timestamp, when an observation happened.
    pub observed_at: Option<String>,
    /// Terminal state for this vertical.
    pub state: VerticalUsageState,
    /// Monetary balances the authenticated endpoint reported alongside the
    /// meters, such as a prepaid credit balance or a pay-as-you-go cap. Empty
    /// when the provider reported none; a balance is never inferred from a
    /// meter, and an absent amount never renders as a zero.
    pub balances: Vec<UsageBalanceView>,
}

/// One provider-reported monetary balance line under a vertical's meters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageBalanceView {
    /// Operator-facing balance name, e.g. `Credits` or `Pay as you go`.
    pub label: String,
    /// Formatted provider amount clause, e.g. `$99.99` or
    /// `$3.55 used of $50.00 cap`.
    pub value: String,
}

/// Terminal state of one vertical's allowance read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerticalUsageState {
    /// An authenticated endpoint supplied these meters.
    Ready {
        /// Provider meters in provider order; empty stays meaningful.
        meters: Vec<UsageMeterView>,
    },
    /// No ready credential. Renders `not connected` text and carries no numbers.
    NotConnected,
    /// A credential exists but no allowance can truthfully be reported.
    Unavailable {
        /// Operator-facing, redaction-safe reason.
        reason: String,
    },
}

/// One allowance meter inside a Ready vertical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageMeterView {
    /// Operator-facing meter name, e.g. `Weekly limit` or `Key usage`.
    pub label: String,
    /// Provider-reported grouping, e.g. `GPT-5.3-Codex-Spark limit`. Meters
    /// sharing a group render as one indented sub-block under its name;
    /// `None` keeps the meter at the vertical's own level.
    pub group: Option<String>,
    /// The provider-reported reading.
    pub reading: UsageMeterReading,
    /// Provider-reported reset, when supplied.
    pub resets_at: Option<String>,
}

/// Reading text for [`UsageMeterReading::Unreported`]: the provider named the
/// period but reported no usage figure for it.
pub const USAGE_NOT_REPORTED: &str = "usage not reported by the provider";

/// A provider-reported meter reading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageMeterReading {
    /// A percentage consumed. `floored_percent` is the whole percent consumed,
    /// truncated the way the provider truncates; `exact_percent` keeps the
    /// provider's own value as text.
    Percent {
        /// Provider percent truncated toward zero, 0 through 100.
        floored_percent: u8,
        /// Provider-reported percent, unrounded, as text.
        exact_percent: String,
        /// Whole percent *remaining*, floored — what the bar fills and the
        /// number states (spec 02 §7.2). `None` until the host projects it, and
        /// the renderer then uses the complement of `floored_percent`.
        percent_left: Option<u8>,
    },
    /// Currency amounts, already formatted. An amount the provider did not
    /// report is absent, never rendered as a zero.
    Amounts {
        /// Label/value pairs in provider order.
        entries: Vec<UsageAmountView>,
    },
    /// The provider named this allowance period (and usually its reset) but
    /// reported no usage figure for it. Renders as words, never as a bar or a
    /// `0%`: the period is a fact, the consumption is unknown.
    Unreported,
}

/// One labelled currency amount.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageAmountView {
    /// Operator-facing amount label, e.g. `today`.
    pub label: String,
    /// Formatted provider amount, e.g. `$0.40`, or `no cap`.
    pub value: String,
}

/// Session token/call ledger for the Limits and Session surfaces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionLedgerView {
    /// Input tokens summed over reporting model operations.
    pub input_tokens: u64,
    /// Provider-reported cached input tokens, included in `input_tokens`.
    pub cached_input_tokens: u64,
    /// Output tokens summed over reporting model operations.
    pub output_tokens: u64,
    /// Reasoning tokens, included in `output_tokens`.
    pub reasoning_tokens: u64,
    /// Model operations observed, including operations that reported no usage.
    pub model_calls: u64,
    /// Model operations that supplied a usage report.
    pub reporting_model_calls: u64,
    /// Host-measured wall time spent in admitted turn dispatch, when any turn
    /// ran. It spans the provider round-trips, local tool execution, and any
    /// approval wait: it is never a provider-reported API duration.
    pub turn_wall_time: Option<String>,
    /// Cost, which is `not available` unless a provider reported one.
    pub cost: UsageCostView,
}

/// Session cost, which is never printed as `$0` when it is unknown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageCostView {
    /// No provider reported a cost. Renders `not available` with this reason.
    NotAvailable {
        /// Why no cost can be stated.
        reason: String,
    },
    /// A provider reported this exact formatted cost.
    Reported {
        /// Formatted provider cost.
        amount: String,
    },
}

impl Default for UsageCostView {
    fn default() -> Self {
        Self::NotAvailable {
            reason: "no provider reported a cost".to_owned(),
        }
    }
}

impl Default for UsageLimitsView {
    /// An empty roster: only the host knows which verticals exist, so the
    /// protocol's default projects none of them rather than inventing four.
    fn default() -> Self {
        Self {
            verticals: Vec::new(),
            session: SessionLedgerView::default(),
            stale_age_seconds: None,
        }
    }
}

/// Host-owned typed session facts for the Usage Session section (spec 02 §5.5).
///
/// Each field is typed host data. The renderer formats it; it never re-parses
/// a rendered string, and an absent field prints `not reported`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionInfoView {
    /// Host binary version.
    pub host_version: String,
    /// Product session id.
    pub session_id: String,
    /// Workspace root the session is bound to.
    pub working_dir: String,
    /// `[agent.<name>]` template the root turn was built from, when named.
    pub root_template: Option<String>,
    /// Root agent model id.
    pub root_model: Option<String>,
    /// Root agent reasoning effort.
    pub root_effort: Option<String>,
    /// What Plak asked the bound route for by way of visible reasoning, in
    /// words: e.g. `reasoning summaries (auto) requested at high`, or `not
    /// offered by <model> at default effort`. Stated from the request policy,
    /// never from whether a thought block happened to render.
    pub thinking: String,
    /// Resolved tool policy: `Direct`, `Code Mode (native-ptc)`, `js-ptc`, or
    /// `py-ptc` (resolved — never `plakous-default`).
    pub tool_policy: Option<String>,
    /// Admitted options, e.g. `effort = medium`, `Fast (extra = priority)`,
    /// `protocol = chat`. Empty means none.
    pub admitted_options: Vec<String>,
    /// API kernel: `Chat Completions`, `Responses`, or `Messages`.
    pub api_kernel: Option<String>,
    /// Admitted root turns in this session.
    pub turn_number: Option<u64>,
    /// Tree epoch admission state (`live`, `interrupted`, `restored_paused`,
    /// `tree_terminal`, `closed`) of the session's durable tree, when the hub
    /// holds one.
    pub tree_epoch: Option<String>,
    /// Context envelope of the bound model, when its contract is known.
    pub context: Option<SessionContextView>,
}

/// The bound model's admitted context envelope (spec 02 §5.6), as typed data.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionContextView {
    /// Raw context window the evidence reports, when it reports one.
    pub reported_window: Option<u64>,
    /// Maximum prepared input the admitted contract accepts.
    pub admitted_input: Option<u64>,
    /// Evidence class label: `release-verified`, `reference-client-envelope`,
    /// or `custom-envelope`.
    pub evidence: String,
}

/// Exact chrome labels for the currently attached product branch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BindingView {
    pub session_id: Option<String>,
    pub account: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub extra: Option<String>,
    pub composition: crate::selection::CompositionSelection,
    pub effective_presentation: String,
}

/// A binding replacement plus an optional truthful transition for inline UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingUpdate {
    pub current: BindingView,
    pub transition: Option<BindingTransition>,
}

impl BindingUpdate {
    #[must_use]
    pub fn rebound(previous: BindingView, current: BindingView) -> Self {
        Self {
            transition: Some(BindingTransition::Rebound {
                previous: Box::new(previous),
                current: Box::new(current.clone()),
            }),
            current,
        }
    }

    #[must_use]
    pub fn model_switched(previous_model: Option<String>, current: BindingView) -> Self {
        Self {
            transition: Some(BindingTransition::ModelSwitched {
                previous_model,
                current_model: current.model.clone(),
            }),
            current,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingTransition {
    Rebound {
        previous: Box<BindingView>,
        current: Box<BindingView>,
    },
    ModelSwitched {
        previous_model: Option<String>,
        current_model: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity(id: &str, label: &str, correlation_id: Option<&str>) -> ActivityItem {
        ActivityItem {
            parent_call_id: None,
            cell_id: None,
            id: id.to_owned(),
            correlation_id: correlation_id.map(str::to_owned),
            kind: ActivityKind::Tool,
            label: label.to_owned(),
            subject: Some("$ cargo test".to_owned()),
            detail: None,
            state: ActivityState::Running,
            started_at_ms: Some(100),
            finished_at_ms: None,
            duration_ms: None,
        }
    }

    #[test]
    fn durable_transcript_payload_ceiling_has_presentation_identity_headroom() {
        let mut snapshot = PresentationSnapshot::empty(1);
        snapshot
            .rows
            .push(TranscriptRow::assistant("x".repeat(16 * 1024 * 1024)));
        assert_eq!(snapshot.validate(), Ok(()));
        snapshot
            .rows
            .push(TranscriptRow::assistant("x".repeat(3 * 1024 * 1024)));
        assert_eq!(snapshot.validate(), Err(PresentationError::ByteLimit));
    }

    #[test]
    fn binding_updates_distinguish_rebind_and_model_switch() {
        let a = BindingView {
            session_id: Some("session-a".into()),
            model: Some("grok-4.6".into()),
            ..BindingView::default()
        };
        let b = BindingView {
            session_id: Some("session-b".into()),
            model: Some("gpt-5.6-sol".into()),
            ..BindingView::default()
        };
        let rebound = BindingUpdate::rebound(a.clone(), b.clone());
        assert!(matches!(
            rebound.transition,
            Some(BindingTransition::Rebound { .. })
        ));
        let switched = BindingUpdate::model_switched(a.model.clone(), b);
        assert!(matches!(
            switched.transition,
            Some(BindingTransition::ModelSwitched { .. })
        ));
    }

    #[test]
    fn activity_labels_name_web_and_mcp_calls() {
        for (name, label) in [
            ("web_search", "searching"),
            ("WebSearch", "searching"),
            ("x_search", "searching"),
            ("web_fetch", "reading"),
            ("WebFetch", "reading"),
            ("fetch", "reading"),
            ("fetch_url", "reading"),
            ("web__run", "reading"),
            ("mcp__docs__search", "calling mcp:docs"),
            ("mcp__git_hub__list_issues", "calling mcp:git_hub"),
            ("mcp__", "working"),
            ("mcp__Docs__x", "working"),
            ("mcp__docs__", "working"),
        ] {
            assert_eq!(activity_label(name, ActivityKind::Tool), label, "{name}");
        }
        assert_eq!(
            activity_label("mcp__docs__search", ActivityKind::Compose),
            "working"
        );
    }

    #[test]
    fn mcp_calls_split_their_exposed_name() {
        assert_eq!(
            McpCall::parse("mcp__git_hub__list__issues"),
            Some(McpCall {
                server: "git_hub",
                tool: "list__issues"
            })
        );
        assert_eq!(McpCall::parse("search_repository"), None);
    }

    #[test]
    fn activity_derives_duration() {
        let mut row = activity("tool-1", "search_repository", None);
        row.state = ActivityState::Succeeded;
        row.finished_at_ms = Some(145);
        assert_eq!(row.with_derived_duration().duration_ms, Some(45));
    }
}
