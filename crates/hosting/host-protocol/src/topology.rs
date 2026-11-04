//! Protocol types for the product agent tree runtime:
//! templates, spawn tiers, sealed candidates, and adoption.
//!
//! Combination rules and UTF-8 byte budgets are product-side after a closed
//! V2-shaped object parse (`deny_unknown_fields`).

use serde::{Deserialize, Deserializer, Serialize};

/// UTF-8 byte ceiling on one `send_agent_message` body.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
/// `wait_agent` timeout when the caller omits one.
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 300_000;
/// `wait_agent` timeout ceiling; a larger request is clamped, not refused.
pub const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
/// Agent ids one `wait_agent` may name: every direct child a node may hold.
pub const MAX_WAIT_TARGETS: usize = MAX_DIRECT_CHILDREN;
/// Byte ceiling on a `submit_result` payload.
pub const MAX_OUTPUT_JSON_BYTES: usize = 262_144;
/// Ceiling on nodes one tree may hold that are not closed, root included:
/// the root plus a full row of direct children. A `recovery_required` node
/// counts until it is resolved and closed.
pub const MAX_TREE_NODES: usize = MAX_DIRECT_CHILDREN + 1;
/// `max_nodes` when the configuration leaves it unset.
pub const DEFAULT_TREE_NODES: usize = 32;
/// Depth one tree may reach, with the root at depth 0.
pub const MAX_TREE_DEPTH: u8 = 4;
/// Ceiling on direct children one node may hold.
pub const MAX_DIRECT_CHILDREN: usize = 64;
/// `max_children` when the configuration leaves it unset.
pub const DEFAULT_DIRECT_CHILDREN: usize = 8;
/// Spawns one tree may hold admitted but not yet started: one batch may
/// start a full row of direct children.
pub const MAX_PENDING_SPAWNS: usize = MAX_DIRECT_CHILDREN;
/// Ceiling on attempts one root prompt may open across every node
/// (`max_attempts`). A tree may lower it; nothing raises it.
pub const MAX_TREE_ATTEMPTS: usize = 256;
/// `max_attempts` when the configuration leaves it unset.
pub const DEFAULT_TREE_ATTEMPTS: usize = 64;
/// Records one node's mailbox may hold; more is `mailbox_full`.
pub const MAX_MAILBOX_RECORDS: usize = 256;
/// Bytes one tree may hold queued across every mailbox.
pub const MAX_QUEUED_MAIL_BYTES: usize = 4 * 1024 * 1024;
/// Byte ceiling on a spawn's description, prompt, and output schema.
pub const MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES: usize = 64 * 1024;
/// Candidate or attempt ids one spawn may start from.
pub const MAX_SPAWN_INPUTS: usize = 8;
/// Templates a working set may name (1–5 is the intended scale).
pub const MAX_WORKING_SET: usize = 5;
/// Depth a grant reaches below its holder when the template names none.
pub const DEFAULT_GRANT_DEPTH: u8 = 1;

/// Model-visible topology tools: agent lifecycle, then occupancy.
pub const TOPOLOGY_PRESENTATION_ORDER: &[&str] = &[
    "spawn_agent",
    "list_agents",
    "send_agent_message",
    "wait_agent",
    "interrupt_agent",
    "close_agent",
    "recover_agent",
    "submit_result",
    "adopt_candidate",
    "discard_workspace",
];

/// Whether a task id is a process id as the process tools write it: `N` or
/// `pN`, nonzero. Any other id in a Grok output or kill call names an agent.
#[must_use]
pub fn is_process_task_id(id: &str) -> bool {
    id.strip_prefix('p')
        .unwrap_or(id)
        .parse::<u32>()
        .is_ok_and(|number| number != 0)
}

/// Whether `tool_id` is a topology registration (`plak.topology.*`).
///
/// A process tool that happens to be spelled like Grok's output or kill tool
/// is not one, so a refused leaf call keeps its own name.
#[must_use]
pub fn is_topology_registration(tool_id: &str) -> bool {
    tool_id.starts_with("plak.topology.")
}

/// The topology slot a call fills, for presentation only (activity rows,
/// wait facts, subjects): a Grok `get_command_or_subagent_output` or a pi
/// `wait` naming a child is the await, and a `kill_command_or_subagent` or a
/// pi `stop` naming one is the interrupt, so they read as `wait_agent` and
/// `interrupt_agent`. Every other call keeps its name (a composition `wait`
/// marker names a `cell_id`, never `ids`). Runtime decisions never read this;
/// they stay on operation identity.
///
/// `topology_call` is the gate: the call's registered id is `plak.topology.*`,
/// or the session presents those slots as Grok's or pi's tools. A refused
/// leaf call is neither, and must not restore as `wait_agent`.
#[must_use]
pub fn presented_slot_name<'a>(
    name: &'a str,
    arguments: Option<&serde_json::Value>,
    topology_call: bool,
) -> &'a str {
    if !topology_call {
        return name;
    }
    let names_agent =
        |id: &serde_json::Value| id.as_str().is_some_and(|id| !is_process_task_id(id));
    match name {
        "get_command_or_subagent_output" | "wait"
            if arguments
                .and_then(|args| args.get(if name == "wait" { "ids" } else { "task_ids" }))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|ids| ids.iter().any(names_agent)) =>
        {
            "wait_agent"
        }
        "kill_command_or_subagent" | "stop"
            if arguments
                .and_then(|args| args.get(if name == "stop" { "id" } else { "task_id" }))
                .is_some_and(names_agent) =>
        {
            "interrupt_agent"
        }
        _ => name,
    }
}

/// The topology slot a settled call filled, for presentation only, where its
/// start could not tell: a bare pi `wait` (no `ids`) on a node granted agent
/// tools is either the process tool's own bare await (`{"tasks": [...]}`) or
/// the tree's await, decided when it ran. The await returns its receipt in the
/// topology envelope `{"result": "<receipt JSON>"}` on a Direct call and raw
/// (`woke_by`, `agents`) in a cell, so such a call reads as `wait_agent` once
/// it settles. `name` is the name its start presented; every other call keeps
/// it. Arguments naming `ids` or a `cell_id` (a composition continuation)
/// rule it out; `None` (a terminal with no arguments at hand) does not.
#[must_use]
pub fn presented_settled_name<'a>(
    name: &'a str,
    arguments: Option<&serde_json::Value>,
    result: Option<&serde_json::Value>,
    topology_call: bool,
) -> &'a str {
    let bare =
        arguments.is_none_or(|args| args.get("ids").is_none() && args.get("cell_id").is_none());
    let awaited = result.is_some_and(|result| {
        result
            .get("result")
            .is_some_and(serde_json::Value::is_string)
            || (result.get("woke_by").is_some() && result.get("agents").is_some())
    });
    if topology_call && name == "wait" && bare && awaited {
        "wait_agent"
    } else {
        name
    }
}

/// `[a-z][a-z0-9-]{0,31}`: the one name grammar for templates.
#[must_use]
pub fn is_lowercase_config_name(name: &str) -> bool {
    let mut chars = name.chars();
    let head_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let tail_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    head_ok && tail_ok && name.len() <= 32
}

/// Names a template may not take: they are `config.toml` section names.
pub const RESERVED_TEMPLATE_NAMES: [&str; 7] = [
    "root",
    "roster",
    "orchestration",
    "tui",
    "compaction",
    "telemetry",
    "agent",
];

/// A user-named agent template, the key of the working set and the only thing
/// a spawn names. Where the host keeps the template files, and
/// in what format, is host data the protocol never sees.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TemplateName(String);

impl TemplateName {
    /// Parse one name, refusing both the grammar and the reserved names.
    ///
    /// # Errors
    ///
    /// Refuses a name outside `[a-z][a-z0-9-]{0,31}` and every reserved name.
    pub fn parse(value: &str) -> Result<Self, String> {
        if !is_lowercase_config_name(value) {
            return Err(format!(
                "invalid template name \"{value}\"; names are [a-z][a-z0-9-]{{0,31}}"
            ));
        }
        if RESERVED_TEMPLATE_NAMES.contains(&value) {
            return Err(format!("template name \"{value}\" is reserved"));
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    /// The name as it appears on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TemplateName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for TemplateName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TemplateName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// `[roster] working_set`: every spawn-capable node's menu is this set minus
/// itself. Bounded to [`MAX_WORKING_SET`] names, no duplicates.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkingSet {
    names: Vec<TemplateName>,
}

impl WorkingSet {
    /// Parse a working set, refusing duplicates and an oversized set.
    ///
    /// # Errors
    ///
    /// Refuses a duplicate name, an invalid name, and a set over
    /// [`MAX_WORKING_SET`] names.
    pub fn parse<I: IntoIterator<Item = S>, S: AsRef<str>>(names: I) -> Result<Self, String> {
        let mut parsed = Vec::new();
        for name in names {
            let name = TemplateName::parse(name.as_ref())?;
            if parsed.contains(&name) {
                return Err(format!(
                    "[roster] working_set names \"{name}\" more than once"
                ));
            }
            parsed.push(name);
        }
        if parsed.len() > MAX_WORKING_SET {
            return Err(format!(
                "[roster] working_set names {} templates; the maximum is {MAX_WORKING_SET}",
                parsed.len()
            ));
        }
        Ok(Self { names: parsed })
    }

    #[must_use]
    /// The parsed names, in the order the configuration gave them.
    pub fn names(&self) -> &[TemplateName] {
        &self.names
    }

    #[must_use]
    /// Whether the set names this template.
    pub fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|item| item.as_str() == name)
    }

    #[must_use]
    /// Whether the set names no template at all.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    #[must_use]
    /// How many templates the set names.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// The spawn menu for a node bound to `self_name`: the set minus itself.
    pub fn iter_excluding<'a>(
        &'a self,
        self_name: Option<&'a str>,
    ) -> impl Iterator<Item = &'a TemplateName> + 'a {
        self.names
            .iter()
            .filter(move |name| self_name != Some(name.as_str()))
    }
}

/// Spawn capability ceiling. Checkout is `workspace` on the spawn, not a
/// side effect of tier: `read_only` refuses Plak file writes, `execute`
/// allows process tools, `mutate` allows file writes.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Process tools allowed; Plak file writes refused.
    ReadOnly,
    /// Process tools allowed; Plak file writes refused.
    Execute,
    /// Process tools and Plak file writes allowed.
    Mutate,
}

impl Tier {
    #[must_use]
    /// The tier's wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Execute => "execute",
            Self::Mutate => "mutate",
        }
    }

    /// Parse a tier from its wire spelling.
    ///
    /// # Errors
    ///
    /// Refuses anything but the three wire spellings.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "read_only" => Ok(Self::ReadOnly),
            "execute" => Ok(Self::Execute),
            "mutate" => Ok(Self::Mutate),
            _ => Err("tier must be read_only|execute|mutate".to_owned()),
        }
    }

    /// Whether this tier can seal a candidate from its own copy.
    ///
    /// Process tools are allowed on every occupancy fence, including inherit
    /// `read_only`. This predicate is the seal gate: `read_only` never owns a
    /// copy that can produce a candidate.
    #[must_use]
    pub const fn allows_process_tools(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    /// Plak file-tool writes are admitted on `mutate` only. `execute` keeps
    /// process tools and refuses file tools, on inherit or on a forked copy.
    #[must_use]
    pub const fn allows_file_writes(self) -> bool {
        matches!(self, Self::Mutate)
    }

    /// Asking above a template's `max_tier` lands at the ceiling; it is not an
    /// error.
    #[must_use]
    pub fn clipped_to(self, ceiling: Self) -> Self {
        self.min(ceiling)
    }
}

/// how a template's prompt states when to delegate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delegation {
    #[default]
    /// The template delegates only when its prompt is told to.
    Explicit,
    /// The template delegates on its own judgment.
    Proactive,
}

impl Delegation {
    #[must_use]
    /// The mode's wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Proactive => "proactive",
        }
    }

    /// Parse a delegation mode from its wire spelling.
    ///
    /// # Errors
    ///
    /// Refuses anything but the two wire spellings.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "explicit" => Ok(Self::Explicit),
            "proactive" => Ok(Self::Proactive),
            _ => Err("delegation must be explicit|proactive".to_owned()),
        }
    }
}

/// Wire spelling is `snake_case`; prompts, JSONL, and docs quote these names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyErrorCode {
    /// The request object carried a field the schema does not define.
    UnknownField,
    /// The request parsed but its field combination is not a legal call.
    SchemaMismatch,
    /// A documented byte or count bound was passed.
    BoundExceeded,
    /// The caller's grant does not reach the named node.
    GrantDenied,
    /// A node addressed itself.
    SelfSend,
    /// The recipient is closed and will not run again.
    RecipientClosed,
    /// The recipient's addressed attempt has already ended.
    RecipientAttemptTerminal,
    /// The recipient is in recovery and cannot be driven.
    RecipientRecoveryRequired,
    /// `purpose=reply` without `in_reply_to`, or `in_reply_to` without it.
    ReplyInvalid,
    /// The recipient's mailbox is at its record or byte bound.
    MailboxFull,
    /// The tree's node or attempt budget is spent.
    SpawnBudget,
    /// Running-node saturation; carries the running set in the message.
    Capacity,
    /// Operator storage is below the new-work floor; never a running-set refusal.
    StorageLow,
    /// A `request_key` was reused for a different brief.
    RequestKeyConflict,
    /// The spawn would pass the tree's depth bound.
    DepthExhausted,
    /// The template exists but this caller's menu does not offer it.
    TemplateNotSpawnable,
    /// No template of that name is configured.
    TemplateUnknown,
    /// No workspace of the required shape could be minted.
    MintUnavailable,
    /// The template's `on_mint` hook exited non-zero or timed out; the copy
    /// was unminted.
    MintHookFailed,
    /// A named input candidate or attempt has not sealed.
    InputNotSealed,
    /// The workspace is not the one the request described.
    WorkspaceIdentityMismatch,
    /// Repository or checkout requires explicit recovery; work is retained.
    WorkspaceRecoveryRequired,
    /// The caller does not own the named workspace.
    WorkspaceNotOwned,
    /// The workspace still backs a candidate or a running node.
    WorkspaceReferenced,
    /// The template names a model this account may not run.
    ModelNotAdmitted,
    /// The root has no parent to submit to.
    SubmitRootForbidden,
    /// The turn token names a turn that has already ended.
    SubmitStaleToken,
    /// The attempt already submitted a different result.
    SubmitDuplicateConflict,
    /// Submission would leave a running child attempt or process unattended.
    LiveWork,
    /// Retained wire code; a tree reopens live, so no current host sends it.
    TreeRestoredPaused,
    /// An earlier operation left this state needing recovery.
    OperationRecoveryRequired,
    /// No candidate of that id exists.
    CandidateUnknown,
    /// The candidate has not sealed, so it cannot be adopted.
    NotSealed,
    /// The caller may not adopt from this subtree.
    NotAuthorized,
    /// The named evidence attempts do not match the candidate.
    EvidenceMismatch,
    /// `expected_target` no longer names the current target snapshot.
    TargetChanged,
    /// A failed adopt result; the child copy is retained.
    MergeConflict,
    /// The candidate is in no state an adopt could act on.
    AdoptNotApplicable,
    /// The workspace is in no state a discard could act on.
    DiscardNotApplicable,
    /// `purpose=followup` reached a node that is not idle.
    FollowupNotIdle,
    /// A closed node cannot accept another attempt.
    FollowupClosed,
    /// An uncertain earlier operation must settle before another attempt.
    FollowupRecoveryRequired,
    /// The child workspace was discarded or removed.
    FollowupWorkspaceGone,
    /// A provider quota refusal blocks the account.
    QuotaBlocked,
    /// `expected_attempt_id` does not name the node's current attempt.
    AttemptMismatch,
    /// A bare `wait_agent` found no managed child with an unreported result.
    NoPendingChildren,
    /// `wait_agent`: the waiting agent's own turn was interrupted or settled,
    /// so the wait is abandoned. Nothing about the watched children changed.
    WaitCallerSettled,
    /// `recover_agent`: the child's session lease is still held by another
    /// owner; retry after the lease period. Like every recovery refusal it
    /// may have advanced the node's session locator and never settles.
    RecoveryLeaseHeld,
    /// `recover_agent`: this process still owns the child's turn, startup, or
    /// retirement; it settles on its own or on the next tree open.
    RecoveryOwnerLive,
    /// `recover_agent`: the session or tree store refused (for example at
    /// capacity). Never settles.
    RecoveryStorageUnavailable,
    /// `recover_agent`: the reopened head needs executor effect evidence this
    /// host does not hold. Never settles.
    RecoveryEffectEvidenceRefused,
    /// `recover_agent`: an interrupted seal, adopt, or removal is all that
    /// keeps the node `recovery_required`; its VCS outcome cannot be decided
    /// exactly, and `close_agent` ends the node.
    RecoveryVcsAmbiguous,
    /// `recover_agent`: the child session could not be reopened or recreated
    /// for another reason the message names. Never settles.
    RecoverySessionUnavailable,
    /// An `inherit` child shares its parent's checkout, so it runs on the
    /// parent's runner; the spawn named another.
    RunnerMismatch,
    /// A `fork` or `snapshot` copy on another runner than the parent's is
    /// not carried yet; nothing was minted.
    CrossRunnerCopyUnavailable,
}

#[derive(Debug, Clone, Eq, PartialEq)]
/// One refusal: a closed code and a bounded human message.
pub struct TopologyError {
    /// The closed wire code a client switches on.
    pub code: TopologyErrorCode,
    /// Bounded prose for a person; never parse it.
    pub message: String,
    /// Optional transient refusal data, independent of the human message.
    pub advice: Option<Box<crate::refusal::RefusalAdvice>>,
    /// Optional semantic repair retained through a composed runtime refusal.
    pub remedy: Option<crate::notice_remedy::NoticeRemedy>,
}

impl TopologyError {
    /// Build one refusal, truncating the message to its 512-byte wire bound.
    #[must_use]
    pub fn new(code: TopologyErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > 512 {
            message.truncate(512);
        }
        Self {
            code,
            message,
            advice: None,
            remedy: None,
        }
    }

    /// Retain structured action-refusal facts without adding command grammar.
    #[must_use]
    pub fn with_advice(mut self, advice: crate::refusal::RefusalAdvice) -> Self {
        self.advice = Some(Box::new(advice));
        self
    }

    /// Preserve typed help when a runtime failure becomes a topology refusal.
    #[must_use]
    pub fn with_failure(mut self, failure: &crate::status::HostFailure) -> Self {
        self.advice.clone_from(&failure.advice);
        self.remedy.clone_from(&failure.remedy);
        self
    }

    /// Serialize the error payload carried inside an ordinary tool result.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        let mut value = serde_json::json!({ "code": self.code, "message": self.message });
        if let Some(advice) = &self.advice {
            value["advice"] = serde_json::json!(advice);
        }
        if let Some(remedy) = &self.remedy {
            value["remedy"] = serde_json::json!(remedy);
        }
        value
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Lifecycle state of one node as serialized in a `tree` row.
pub enum NodeStatus {
    /// Admitted; its first attempt has not started.
    Pending,
    /// An attempt is driving now.
    Running,
    /// Held by a paused tree.
    Paused,
    /// Turn ended; the candidate is being snapshotted outside the tree mutex.
    /// Not settled, not finished.
    Sealing,
    /// The last attempt ended with a result.
    Completed,
    /// The last attempt was interrupted; the node can be resumed.
    Interrupted,
    /// The last attempt ended without a result.
    Failed,
    /// Terminal: the node will never run again.
    Closed,
    /// Terminal: the node's state needs recovery before anything else.
    RecoveryRequired,
}

/// What a node is *doing*, as opposed to which row it last wrote.
///
///
/// `NodeStatus` has nine values because nine things can be written down; a
/// caller almost never wants nine. Every "is it still going?" question in the
/// tree is one of the five here, and [`NodeStatus::disposition`] is the single
/// place the mapping is stated — so `is_terminal`, `is_finished`, and
/// `settles_a_wait` cannot drift into three different readings of the same
/// status, which is what they had done.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeDisposition {
    /// A turn is running, or one will without anybody asking: `Pending`,
    /// `Running`, `Paused`.
    Live,
    /// The turn ended and the candidate is being snapshotted outside the tree
    /// mutex. Not settled, not finished.
    Sealing,
    /// Nothing is running and nothing starts without a re-task, but a
    /// `followup` can still re-task it: `Completed`, `Failed`.
    Idle,
    /// Cancelled mid-turn. Resumable, so not idle; but the wait that is open
    /// now will get nothing, so it settles one.
    Interrupted,
    /// It will never run again: `Closed`, `RecoveryRequired`.
    Dead,
}

impl NodeDisposition {
    /// The disposition's wire spelling, identical to its serde name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Sealing => "sealing",
            Self::Idle => "idle",
            Self::Interrupted => "interrupted",
            Self::Dead => "dead",
        }
    }
}

impl NodeStatus {
    /// What this status means the node is doing. The one mapping; every
    /// predicate below reads it.
    #[must_use]
    pub const fn disposition(self) -> NodeDisposition {
        match self {
            Self::Pending | Self::Running | Self::Paused => NodeDisposition::Live,
            Self::Sealing => NodeDisposition::Sealing,
            Self::Completed | Self::Failed => NodeDisposition::Idle,
            Self::Interrupted => NodeDisposition::Interrupted,
            Self::Closed | Self::RecoveryRequired => NodeDisposition::Dead,
        }
    }

    /// with `followup` able to re-task a finished node, terminal
    /// means the node can never run again.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self.disposition(), NodeDisposition::Dead)
    }

    /// The node has no turn running and none will start without a re-task:
    /// the old "terminal" meaning, kept for listing filters, epoch quiescence,
    /// and detach accounting.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(
            self.disposition(),
            NodeDisposition::Idle | NodeDisposition::Dead
        )
    }

    /// Whether a `wait_agent` on this node has nothing left to wait for.
    ///
    /// An interrupted child is not finished — it can be resumed — but it is not
    /// going to produce a result for the wait that is open now. A `Sealing`
    /// child has not settled: its candidate is still being written.
    #[must_use]
    pub const fn settles_a_wait(self) -> bool {
        matches!(
            self.disposition(),
            NodeDisposition::Idle | NodeDisposition::Dead | NodeDisposition::Interrupted
        )
    }

    /// The status's wire spelling, identical to its serde name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Sealing => "sealing",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Closed => "closed",
            Self::RecoveryRequired => "recovery_required",
        }
    }
}

/// Whether the attached operator may confirm `u` for this child.
///
/// The same predicate the host refuses with: a non-root `failed` or
/// `recovery_required` child that still has an attempt, on a tree
/// `recover_agent` would still accept (`live`, `interrupted`, or
/// `tree_terminal`). A paused or closed tree, the root, and any other
/// status are not eligible. This is not a model grant and not a revision
/// fence.
#[must_use]
pub fn operator_child_recover_eligible(
    tree_state: &str,
    node_status: &str,
    is_root: bool,
    has_attempt: bool,
) -> bool {
    if is_root || !has_attempt {
        return false;
    }
    matches!(tree_state, "live" | "interrupted" | "tree_terminal")
        && matches!(node_status, "failed" | "recovery_required")
}

/// Status of one attempt of one node (the attempt is the unit).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    /// Driving now.
    Running,
    /// The child submitted a result; the turn is finishing.
    Submitted,
    /// Ended with a result.
    Completed,
    /// The turn ended without a `submit_result`.
    CompletedWithoutSubmission,
    /// Ended on an error.
    Failed,
    /// Interrupted or closed before it could finish.
    Cancelled,
    /// Left in a state needing recovery.
    RecoveryRequired,
    /// Ended because the node was closed.
    Closed,
    /// A provider quota refusal ended the attempt.
    Blocked,
    /// The attempt's candidate was adopted by an ancestor.
    Adopted,
}

impl AttemptStatus {
    #[must_use]
    /// Whether the attempt has stopped and cannot resume.
    ///
    /// Deliberately not called `is_terminal`: a *node* is terminal when it can
    /// never run again, and an attempt that stopped leaves a node that a
    /// `followup` can re-task. Two words for two facts.
    pub const fn is_stopped(self) -> bool {
        !matches!(self, Self::Running | Self::Submitted)
    }

    /// The status's wire spelling, identical to its serde name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Submitted => "submitted",
            Self::Completed => "completed",
            Self::CompletedWithoutSubmission => "completed_without_submission",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RecoveryRequired => "recovery_required",
            Self::Closed => "closed",
            Self::Blocked => "blocked",
            Self::Adopted => "adopted",
        }
    }
}

/// `<agent_id>-a<seq>`: one attempt of one node. Stays inside `valid_id`'s
/// alphabet so it can travel anywhere an agent id can.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttemptId(String);

impl AttemptId {
    #[must_use]
    /// Build the id for one attempt of one node.
    pub fn new(agent_id: &str, seq: u32) -> Self {
        Self(format!("{agent_id}-a{seq}"))
    }

    /// Rebuilds an id from its persisted text; the store row is authority.
    #[must_use]
    pub fn from_persisted(text: String) -> Self {
        Self(text)
    }

    #[must_use]
    /// The id as it appears on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttemptId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// `cand-<attempt>`: the sealed snapshot one attempt produced.
///
/// A newtype because the id is minted in one place and then travels through a
/// tool receipt, an attempt row, a candidate row, and a git ref; as a `String`
/// it was formatted at the mint site and re-formatted at the ref site, and
/// nothing said the two had to agree. `#[serde(transparent)]`, so the wire
/// bytes are the same string they always were.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CandidateId(String);

impl CandidateId {
    /// The candidate one attempt seals. The attempt is the unit of work,
    /// so it is what names the snapshot.
    #[must_use]
    pub fn for_attempt(attempt: &AttemptId) -> Self {
        Self(format!("cand-{attempt}"))
    }

    /// The candidate a node with no attempt row seals — the root's own copy.
    #[must_use]
    pub fn for_agent(agent_id: &str) -> Self {
        Self(format!("cand-{agent_id}"))
    }

    /// Rebuilds an id from its persisted text or from a model's request; the
    /// stored row is authority and an unknown id is refused by lookup, not by
    /// shape.
    #[must_use]
    pub fn from_persisted(text: String) -> Self {
        Self(text)
    }

    /// The id as it appears on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CandidateId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Lifecycle of one node's workspace as serialized in a `tree` row.
pub enum WorkspaceStatus {
    /// In use by its node.
    Active,
    /// A candidate from this copy is being landed outside the tree mutex.
    /// Reopening a tree in this state is `RecoveryRequired`.
    Adopting,
    /// Removal is pending or requires recovery; the copy is not yet discarded.
    Discarding,
    /// Reclaimed with nothing to land.
    CleanRemoved,
    /// Its candidate was landed by an ancestor.
    Adopted,
    /// Dropped without landing.
    Discarded,
}

impl WorkspaceStatus {
    #[must_use]
    /// Whether a landing or removal is unresolved outside the tree mutex.
    pub const fn is_in_flight(self) -> bool {
        matches!(self, Self::Adopting | Self::Discarding)
    }

    /// The status's wire spelling, identical to its serde name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Adopting => "adopting",
            Self::Discarding => "discarding",
            Self::CleanRemoved => "clean_removed",
            Self::Adopted => "adopted",
            Self::Discarded => "discarded",
        }
    }
}

/// What a tree admits right now. The `state` label on a `tree`
/// frame, and the fence every mutating tool consults.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeAdmissionState {
    /// Ordinary operation.
    Live,
    /// A root interrupt fences mutating work until it is lifted.
    Interrupted,
    /// Retained stored and wire spelling of a state no tree enters any more: a
    /// tree reopens live, and a stored `restored_paused` reads as live.
    RestoredPaused,
    /// Every descendant is terminal; the root may be rebound.
    TreeTerminal,
    /// The tree is done.
    Closed,
}

impl TreeAdmissionState {
    /// The state's wire spelling, identical to its serde name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Interrupted => "interrupted",
            Self::RestoredPaused => "restored_paused",
            Self::TreeTerminal => "tree_terminal",
            Self::Closed => "closed",
        }
    }
}

/// `[orchestration] occupancy`. `single_lease` is the only writer mode.
///
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterMode {
    #[default]
    /// One writer at a time, taken by lease.
    SingleLease,
}

impl WriterMode {
    #[must_use]
    /// The mode's wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SingleLease => "single_lease",
        }
    }

    /// Parse a writer mode.
    ///
    /// # Errors
    ///
    /// Refuses anything other than `single_lease`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "single_lease" => Ok(Self::SingleLease),
            _ => Err("occupancy must be single_lease".to_owned()),
        }
    }
}

/// Workspace shape recorded on the child after spawn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// The parent's checkout, live.
    #[default]
    Inherit,
    /// A disposable copy of a snapshotted base.
    Snapshot,
    /// Own worktree forked from a snapshotted base.
    Fork,
}

impl WorkspaceMode {
    /// The mode's wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Snapshot => "snapshot",
            Self::Fork => "fork",
        }
    }
}

/// Spawn-time checkout choice. Omit or `inherit` shares the caller's cwd;
/// `fork` mints an isolated copy (snapshot for non-mutate, worktree for mutate).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnWorkspace {
    /// Same checkout as the parent.
    #[default]
    Inherit,
    /// Isolated workspace off the parent checkout.
    Fork,
}

impl SpawnWorkspace {
    /// True when the spawn shares the caller's checkout.
    #[must_use]
    pub const fn is_inherit(&self) -> bool {
        matches!(self, Self::Inherit)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Why one node wrote to another; it decides the delivery rules.
pub enum MessagePurpose {
    #[default]
    /// Ordinary traffic between peers.
    Coordinate,
    /// A result-shaped observation worth the recipient's attention.
    Finding,
    /// Expects a `reply`.
    Question,
    /// Answers a `question`; requires `in_reply_to`.
    Reply,
    /// Re-task an idle finished child on a new attempt.
    Followup,
    /// Host-authored pushed completion; refused from a model.
    Result,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// How soon a delivered message interrupts the recipient.
pub enum MessagePriority {
    #[default]
    /// Delivered at the recipient's next turn boundary.
    Deferred,
    /// Delivered as soon as the recipient can take it.
    Urgent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// `spawn_agent` arguments.
pub struct SpawnAgentRequest {
    /// The template the child is bound to; the only thing a spawn names.
    pub template: TemplateName,
    #[serde(default)]
    /// One line naming the child's job, shown in the tree.
    pub description: String,
    /// The child's brief.
    pub prompt: String,
    /// JSON Schema the child's `submit_result` must satisfy.
    pub output_schema_json: String,
    /// Requested tier; a template ceiling clips it rather than refusing.
    pub tier: Tier,
    /// Checkout: omit or `inherit` shares the parent cwd; `fork` isolates.
    #[serde(default, skip_serializing_if = "SpawnWorkspace::is_inherit")]
    pub workspace: SpawnWorkspace,
    /// Candidate or attempt ids the child starts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<String>>,
    /// Caller key recorded on the attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<String>,
}

impl SpawnAgentRequest {
    /// Shape rules only. The `max_tier` clip is not a validation error and
    /// happens in `spawn_agent`.
    ///
    /// # Errors
    ///
    /// Refuses an empty prompt or schema, more than [`MAX_SPAWN_INPUTS`] inputs,
    /// an empty input id, and any input on an inherit spawn.
    pub fn validate(&self) -> Result<(), TopologyErrorCode> {
        if self.prompt.is_empty() || self.output_schema_json.is_empty() {
            return Err(TopologyErrorCode::SchemaMismatch);
        }
        let inputs = self.inputs.as_deref().unwrap_or_default();
        if inputs.len() > MAX_SPAWN_INPUTS || inputs.iter().any(String::is_empty) {
            return Err(TopologyErrorCode::SchemaMismatch);
        }
        // A candidate can only be given a minted copy; inherit has none.
        if self.workspace.is_inherit() && !inputs.is_empty() {
            return Err(TopologyErrorCode::SchemaMismatch);
        }
        Ok(())
    }

    #[must_use]
    /// The named inputs, or an empty slice when the spawn named none.
    pub fn input_ids(&self) -> &[String] {
        self.inputs.as_deref().unwrap_or_default()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SpawnAgentRequest {
    const TEST_SCHEMA: &str = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}"#;

    /// # Panics
    ///
    /// Panics when `template` is not a valid template name.
    /// One spawn request with a fixed object schema, for tests.
    ///
    /// # Panics
    ///
    /// Panics when `template` is not a valid template name.
    #[must_use]
    pub fn test_named(template: &str, tier: Tier, description: &str, prompt: &str) -> Self {
        Self {
            template: match TemplateName::parse(template) {
                Ok(name) => name,
                // Fixture builder: callers pass a literal, valid template name.
                Err(error) => unreachable!("invalid test template name: {error}"),
            },
            description: description.into(),
            prompt: prompt.into(),
            output_schema_json: Self::TEST_SCHEMA.into(),
            tier,
            workspace: SpawnWorkspace::Inherit,
            inputs: None,
            request_key: None,
        }
    }

    /// Same as [`Self::test_named`], with `workspace=fork`.
    #[must_use]
    pub fn with_fork(mut self) -> Self {
        self.workspace = SpawnWorkspace::Fork;
        self
    }

    /// A `read_only` reviewer spawn, for tests.
    #[must_use]
    pub fn test_review() -> Self {
        Self::test_named("review", Tier::ReadOnly, "reviewer", "review the diff")
    }

    /// A `read_only` supervisor spawn, for tests.
    #[must_use]
    pub fn test_orchestrate() -> Self {
        Self::test_named(
            "orchestrate",
            Tier::ReadOnly,
            "supervisor",
            "Coordinate implement and review children for src/parser.rs and return a summary.",
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// `send_agent_message` arguments.
pub struct SendAgentMessageRequest {
    /// Follow-up only: merge the parent's live state when clean; false opts out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<bool>,
    /// The recipient.
    pub agent_id: String,
    /// The body, bounded by [`MAX_MESSAGE_BYTES`].
    pub message: String,
    /// Delivery rules; `coordinate` when omitted.
    pub purpose: Option<MessagePurpose>,
    /// Delivery urgency; urgent from a parent to its running child, deferred otherwise.
    pub priority: Option<MessagePriority>,
    /// The message this answers; required by, and only by, `reply`.
    pub in_reply_to: Option<String>,
    /// Only with `purpose=followup`: a new result schema for the new attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema_json: Option<String>,
}

impl SendAgentMessageRequest {
    /// Shape and combination rules for one sender.
    ///
    /// # Errors
    ///
    /// Refuses a self-send, a body outside 1..=[`MAX_MESSAGE_BYTES`], the
    /// host-only `result` purpose, a `reply`/`in_reply_to` mismatch, and a
    /// schema on anything but a `followup`.
    pub fn validate(&self, sender_id: &str) -> Result<(), TopologyErrorCode> {
        if self.agent_id == sender_id {
            return Err(TopologyErrorCode::SelfSend);
        }
        if self.message.is_empty() || self.message.len() > MAX_MESSAGE_BYTES {
            return Err(TopologyErrorCode::BoundExceeded);
        }
        let purpose = self.purpose.unwrap_or(MessagePurpose::Coordinate);
        // `result` is host-authored pushed completion; a model never sends it.
        if purpose == MessagePurpose::Result {
            return Err(TopologyErrorCode::SchemaMismatch);
        }
        let in_reply_to = self
            .in_reply_to
            .as_deref()
            .filter(|value| !value.is_empty());
        if purpose == MessagePurpose::Reply && in_reply_to.is_none() {
            return Err(TopologyErrorCode::ReplyInvalid);
        }
        if purpose != MessagePurpose::Reply && in_reply_to.is_some() {
            return Err(TopologyErrorCode::ReplyInvalid);
        }
        if purpose != MessagePurpose::Followup
            && (self.output_schema_json.is_some() || self.refresh.is_some())
        {
            return Err(TopologyErrorCode::SchemaMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// `list_agents` arguments.
pub struct ListAgentsRequest {
    /// Include acknowledged history, newest first within the mailbox bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_read: Option<bool>,
    /// Include the caller's own row.
    pub include_self: Option<bool>,
    /// Include rows for nodes that have finished.
    pub include_completed: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// `wait_agent` arguments.
pub struct WaitAgentRequest {
    /// Omitted means every child the caller manages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_json_u64")]
    /// Wait ceiling; [`DEFAULT_WAIT_TIMEOUT_MS`] when omitted, clamped to
    /// [`MAX_WAIT_TIMEOUT_MS`].
    pub timeout_ms: Option<u64>,
}

impl WaitAgentRequest {
    /// Shape rules; returns the clamped timeout the wait will use.
    ///
    /// # Errors
    ///
    /// Refuses an empty, oversized, or duplicated target list and a zero
    /// timeout.
    pub fn validate(&self) -> Result<u64, TopologyErrorCode> {
        if let Some(ids) = &self.agent_ids {
            if ids.is_empty() || ids.len() > MAX_WAIT_TARGETS {
                return Err(TopologyErrorCode::SchemaMismatch);
            }
            let mut seen = std::collections::BTreeSet::new();
            for id in ids {
                if !seen.insert(id.as_str()) {
                    return Err(TopologyErrorCode::SchemaMismatch);
                }
            }
        }
        let timeout = self.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        if timeout < 1 {
            return Err(TopologyErrorCode::SchemaMismatch);
        }
        Ok(timeout.min(MAX_WAIT_TIMEOUT_MS))
    }
}

/// `interrupt_agent` / `close_agent` / `recover_agent`: optionally fenced to
/// one attempt.
///
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptScopedRequest {
    /// The node acted on.
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// When given, fences the call to that one attempt.
    pub expected_attempt_id: Option<AttemptId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// `submit_result` arguments.
pub struct SubmitResultRequest {
    #[serde(deserialize_with = "deserialize_json_u64")]
    /// The token of the turn submitting; a stale one is refused.
    pub turn_token: u64,
    /// The result, which must satisfy the spawn's output schema.
    pub output_json: String,
}

fn deserialize_json_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    json_u64(&serde_json::Value::deserialize(deserializer)?)
}

fn deserialize_optional_json_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => json_u64(&value).map(Some),
    }
}

fn json_u64<E: serde::de::Error>(value: &serde_json::Value) -> Result<u64, E> {
    if let Some(value) = value.as_u64() {
        return Ok(value);
    }
    let value = value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0 && value.fract() == 0.0)
        .ok_or_else(|| E::custom("expected an exact non-negative integer"))?;
    value
        .to_string()
        .parse::<u64>()
        .map_err(|_| E::custom("integer exceeds u64"))
}

impl SubmitResultRequest {
    /// Shape rules; the root may never submit.
    ///
    /// # Errors
    ///
    /// Refuses a submit from the root, a zero token, and an output over
    /// [`MAX_OUTPUT_JSON_BYTES`].
    pub fn validate(&self, caller_is_root: bool) -> Result<(), TopologyErrorCode> {
        if caller_is_root {
            return Err(TopologyErrorCode::SubmitRootForbidden);
        }
        if self.turn_token < 1 {
            return Err(TopologyErrorCode::SchemaMismatch);
        }
        if self.output_json.len() > MAX_OUTPUT_JSON_BYTES {
            return Err(TopologyErrorCode::BoundExceeded);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// `discard_workspace` arguments.
pub struct WorkspaceIdRequest {
    /// The workspace to drop.
    pub workspace_id: String,
}

/// One path a three-way merge could not resolve, with the VCS conflict kind.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictPath {
    /// Path named by the merge, relative to the checkout.
    pub path: String,
    /// First non-contents conflict kind, otherwise contents; binary wins.
    pub kind: String,
}

/// Land immutable work into the caller's checkout.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptCandidateRequest {
    /// Undo this adoption's delta, preserving later target edits.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revert: bool,
    /// Explicitly allow conflict markers in the receiving checkout.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accept_conflicts: bool,
    /// The sealed candidate to land.
    pub candidate_id: CandidateId,
    /// When given, must equal the current target snapshot or the call returns
    /// `target_changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_target: Option<String>,
    /// Review attempts recorded as evidence for this adoption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<String>>,
}

/// Successful adoption receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AdoptOutcome {
    /// This receipt reversed an adoption rather than landing new work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverted: bool,
    /// The candidate that was landed.
    pub candidate_id: CandidateId,
    /// The target snapshot before the landing.
    pub target_before: String,
    /// The target snapshot after it.
    pub target_after: String,
    /// Files the landing touched.
    pub files_changed: u32,
    /// Changed paths, `"M src/lib.rs"` or `"R old.rs -> new.rs"`, at most 64.
    /// Absent on a receipt from a build that did not list them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// `files` omitted paths past the 64-entry bound. `files_changed` is the
    /// full count either way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub files_truncated: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// What one node may see and do, derived from its place in the tree.
pub struct TopologyGrant {
    /// Nodes this node may see in `list_agents`.
    pub visible_ids: Vec<String>,
    /// Nodes this node may write to.
    pub message_peers: Vec<String>,
    /// Nodes this node may interrupt or close.
    pub managed_ids: Vec<String>,
    /// Whether this node may spawn at all.
    pub may_spawn: bool,
    /// May land sealed candidates from its own subtree into its own copy.
    /// Meaningless without `may_spawn`: there is no subtree.
    pub may_adopt: bool,
    /// Depth this node's subtree may still reach.
    pub max_depth: u8,
    /// Nodes this node may still add.
    pub max_nodes: u8,
    /// Direct children per node this epoch. May only lower the appendix 8.
    pub max_children: u8,
    /// Nodes this node may interrupt.
    pub interrupt_ids: Vec<String>,
    /// Nodes this node may close.
    pub close_ids: Vec<String>,
}

impl TopologyGrant {
    /// Rewrite parent and child grants for a spawn that was admitted.
    pub fn apply_spawn(parent: &mut Self, child: &mut Self, parent_id: &str, child_id: &str) {
        if !parent.message_peers.iter().any(|id| id == child_id) {
            parent.message_peers.push(child_id.to_owned());
        }
        if !parent.visible_ids.iter().any(|id| id == child_id) {
            parent.visible_ids.push(child_id.to_owned());
        }
        if !parent.managed_ids.iter().any(|id| id == child_id) {
            parent.managed_ids.push(child_id.to_owned());
        }
        child.message_peers = vec![parent_id.to_owned()];
        // Authority comes from the child's template, applied by the spawner.
        child.may_spawn = false;
        child.may_adopt = false;
        child.interrupt_ids.clear();
        child.close_ids.clear();
    }

    /// Catalog families for one node's frozen nested-eligible set.
    ///
    /// `send_agent_message` is included for `may_spawn` even before peers exist
    /// because a session catalog cannot grow after the first spawn.
    #[must_use]
    pub fn compiled_tool_names(&self, is_root: bool, remaining_depth: bool) -> Vec<&'static str> {
        let spawn_live = self.may_spawn && remaining_depth;
        let has_peers = self.may_spawn || !self.message_peers.is_empty();
        TOPOLOGY_PRESENTATION_ORDER
            .iter()
            .copied()
            .filter(|name| match *name {
                "spawn_agent" | "wait_agent" | "interrupt_agent" | "close_agent"
                | "recover_agent" => spawn_live,
                "list_agents" | "send_agent_message" => has_peers,
                "submit_result" => !is_root,
                "adopt_candidate" => self.may_adopt,
                "discard_workspace" => self.may_spawn,
                _ => false,
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
