//! The product session record: the queue, the transcript projection, the
//! submission and workflow intents, and the CAS discipline they are committed
//! under.
//!
//! Plain data and its rules. *Where* a record is persisted is the product's:
//! the SQLite implementation of `SessionRecordStore` and the free functions
//! that open `product.sqlite3` live in `bin/plak`, and
//! nothing here names a path.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use plakous::model::{now_unix_secs, sha256_hex};
use plakous::sessions::SessionLocator;

use crate::binding::{
    CompositionSelection as ProductCompositionSelection, TargetSelection as ProductTargetSelection,
};

mod journal;
mod node;
pub mod projector;
pub(crate) mod queue;
mod successor;
mod transcript;
pub use journal::{
    ProductCompactReason, RebindSelection, ResultMailBatch, SessionWorkflowKind, SwitchSelection,
};
pub use node::{RootProfile, RunnerDisplay, SessionNode};
pub use plakous_host_protocol::views::transcript::{
    AutoContinue, NoticePlacement, NoticeScope, NoticeTone, ThinkingRow, ToolItem, TranscriptRow,
};
pub use queue::{QueueCommands, QueueDrainState, QueueItem};
pub(crate) use transcript::bound_row;
pub use transcript::named_image_chip;
pub(crate) use transcript::provisional_key;
pub use transcript::{ProductTranscript, TRANSCRIPT_TRUNCATION_MARKER};
pub(crate) use transcript::{draft_image_chip, image_chip, with_image_chips};

/// Product transcript is a bounded UI/read-model projection, never canonical
/// conversation or recovery authority.
pub const MAX_TRANSCRIPT_LINES: usize = 10_000;
pub const MAX_TRANSCRIPT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PENDING_PROMPTS: usize = 256;
pub const MAX_PROMPT_BYTES: usize = 1024 * 1024;
pub const MAX_PROMPT_HISTORY: usize = 500;
/// Keeps the fast product shelf useful without allowing 500 individually
/// valid 1 MiB prompts to retain 500 MiB in every session.
pub const MAX_PROMPT_HISTORY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_PENDING_QUEUE_BYTES: usize = 16 * 1024 * 1024;
/// Raw image bytes all queued items of one session may hold together.
pub const MAX_PENDING_QUEUE_IMAGE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_TOOL_ACTIVITY_DETAIL_BYTES: usize = 2 * 1024;
/// Same ceiling as the host's pending-reasoning buffer: one operator-visible
/// thought block, never model-facing history.
pub const MAX_THINKING_ACTIVITY_BYTES: usize = 64 * 1024;

pub fn new_session_create_operation_id(workflow_operation_id: &str) -> Result<String, String> {
    validate_portable_identity(workflow_operation_id, "new session workflow operation")?;
    let operation = format!("{workflow_operation_id}:create");
    validate_portable_identity(&operation, "new session create operation")?;
    Ok(operation)
}

pub(crate) fn bound_subject(value: &str) -> String {
    bound_subject_to(value, 240)
}

/// A tool or process subject as the live path stores it: redact, then bound
/// to one line (spec 04 §5). The result is a fixed point of this function, so
/// import runs the same function rather than a second copy.
pub(crate) fn live_tool_subject(value: &str) -> String {
    redaction_fixed_point(
        bound_subject(&crate::session_log::redact_text_preview(value)),
        "…",
    )
}

/// A tool row's detail as stored: redacted, then bounded to
/// [`MAX_TOOL_ACTIVITY_DETAIL_BYTES`], as a fixed point of this function.
pub(crate) fn bound_tool_detail(value: &str) -> String {
    redaction_fixed_point(
        bound_activity(
            plakous::tools::preview_redaction::text(value),
            MAX_TOOL_ACTIVITY_DETAIL_BYTES,
        ),
        TRUNCATED_ACTIVITY_MARKER,
    )
}

/// `bounded` if redaction leaves it unchanged, else the longest cut of it
/// ending in `marker` that redaction leaves unchanged. A cut or a folded line
/// can complete a shape the redactor did not match before (`AKIA` and sixteen
/// characters before the marker, `Cookie: ` before an ellipsis), so without
/// this a second redact-and-bound pass could change a stored row. The cut only
/// shrinks, so the bound still holds, and it ends: the marker alone matches no
/// rule.
fn redaction_fixed_point(bounded: String, marker: &str) -> String {
    let redact = plakous::tools::preview_redaction::text;
    if redact(&bounded) == bounded {
        return bounded;
    }
    let body = bounded.strip_suffix(marker).unwrap_or(&bounded);
    let mut end = body.len();
    loop {
        end = body[..end]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        let candidate = format!("{}{marker}", body[..end].trim_end_matches(marker));
        if end == 0 || redact(&candidate) == candidate {
            return candidate;
        }
    }
}

pub(crate) fn bound_subject_to(value: &str, limit: usize) -> String {
    let (value, truncated) = value
        .strip_suffix("\n[preview truncated]")
        .map_or((value, false), |value| (value, true));
    let value = value.replace("\r\n", "↵").replace(['\r', '\n'], "↵");
    if value.len() <= limit && !truncated {
        return value;
    }
    let mut end = value.len().min(limit.saturating_sub("…".len()));
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", value[..end].trim_end_matches('…'))
}

pub(crate) const TRUNCATED_ACTIVITY_MARKER: &str = "\n[preview truncated]";

fn bound_activity(value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let marker = TRUNCATED_ACTIVITY_MARKER;
    let mut end = maximum.saturating_sub(marker.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &value[..end])
}

/// Lineage operation prefix of a session imported from another build's
/// export document. The source session it names lives under the state key
/// that follows the prefix, never in this build's store, so a lineage walk
/// stops at it.
pub const CARRY_FORWARD_OPERATION_PREFIX: &str = "plak:carry-forward:";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecord {
    /// Product-store CAS token: the revision this record was loaded at or last
    /// committed with, `0` before its first commit. Plain data the store reads
    /// and advances; it is never product meaning, never sent to Plakous and
    /// never rendered.
    pub store_revision: u64,
    pub id: String,
    /// The agent node this session is, written at creation: the root of a
    /// tree of its own, or the source's node for a successor that stays on
    /// the source's tree (spec 02 §2.6). Never rewritten.
    pub node: SessionNode,
    /// Stable product-owned identity reused to reconcile runtime creation.
    pub create_operation_id: String,
    pub workspace_root: PathBuf,
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
    pub account: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub composition: ProductCompositionSelection,
    /// Exact product target selected before construction. This is never
    /// reconstructed from account/model strings or current defaults.
    pub target_selection: ProductTargetSelection,
    pub title: Option<String>,
    /// Product-only lineage for branch presentation; Plakous remains canonical authority.
    pub source_product_session_id: Option<String>,
    pub source_workflow_operation_id: Option<String>,
    /// Exact immutable Plakous checkpoint currently bound to this product session.
    pub plakous_locator: Option<SessionLocator>,
    /// The root's optional tools, frozen when the runtime is first created
    /// and written once with the locator; successors keep the source's.
    /// `None` before that creation. A workspace listing leaves it `None`.
    pub frozen_tools: Option<crate::frozen_tools::FrozenRootTools>,
    /// Bounded transcript lines (user/assistant/tool summaries) and the row
    /// writes the store has yet to commit.
    pub transcript: ProductTranscript,
    /// The session's first user prompt, bounded, as projected by a workspace
    /// listing. It is a read projection for pickers and lists, never a durable
    /// column: loading or creating a record leaves it `None`.
    pub first_prompt: Option<String>,
    /// Product-only composer history, oldest to newest. This is neither
    /// reconstructed from the transcript nor used as Plakous history.
    pub prompt_history: Vec<String>,
    /// Pending user queue (survives resume with stable item identity).
    pub pending_queue: Vec<QueueItem>,
    /// Read-only, process-local submission/fact join for queue decisions.
    pub queue_commands: QueueCommands,
    /// An acknowledged command's fact has not caught up in this live process.
    /// Never persisted; reopen projects before retrying immutable submissions.
    pub queue_projection_debt: bool,
    /// Process-local permission to automatically deliver queued work; never stored.
    pub queue_drain_state: QueueDrainState,
    /// Admitted root turns, derived from the feed by the projector.
    pub root_turn_count: u64,
    /// Process-local cache of the joined wake-suppression intents and facts.
    pub child_wake_suppressed: bool,
    /// Distinct tool activity rows the host has committed for this session:
    /// one per call id, rejected preparations included, never per state
    /// change. Durable and monotonic like `root_turn_count`, so the Context
    /// tab's `Tool calls` survives a resume.
    pub tool_call_count: u64,
    /// Compactions that committed a successor checkpoint for this session.
    pub compaction_count: u64,
    /// A same-store carry written ahead of its runtime create: set, with no
    /// locator, before the create, and cleared in the save that binds the
    /// locator. Reconciled on open and at startup (spec 02 §2.6).
    pub carry_intent: Option<CarryIntent>,
    /// The profile of a root whose tree has no rows yet (a virtual tree of
    /// one). `None` today: every session's profile is its tree's.
    pub root_profile: Option<RootProfile>,
    /// A display copy of the runner and workspace the root runs on; `None`
    /// is the local runner at `workspace_root`, which is every session today.
    pub runner: Option<RunnerDisplay>,
}

/// The write-ahead marker of a same-store carry (spec 02 §2.5.1, §2.6):
/// enough to finish, replay or withdraw the carry after a crash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CarryIntent {
    /// The source branch the carry continues, as its record named it.
    pub source: SessionLocator,
    /// Whether the carry continues the source's agent line.
    pub continues_line: bool,
    /// The notice the carry records on its successor.
    pub cause: String,
    /// The orchestration carry of a reload, which has no root fallback.
    pub orchestration: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CarryIntentWire {
    source: String,
    continues_line: bool,
    cause: String,
    orchestration: bool,
}

impl CarryIntent {
    /// The stored encoding.
    ///
    /// # Errors
    /// Returns a message when the intent does not serialize.
    pub fn encode(&self) -> Result<String, String> {
        serde_json::to_string(&CarryIntentWire {
            source: self.source.to_string(),
            continues_line: self.continues_line,
            cause: self.cause.clone(),
            orchestration: self.orchestration,
        })
        .map_err(|_| "carry intent is not serializable".to_owned())
    }

    /// Decodes the stored encoding.
    ///
    /// # Errors
    /// Returns a message for anything this build did not write.
    pub fn decode(text: &str) -> Result<Self, String> {
        let wire: CarryIntentWire =
            serde_json::from_str(text).map_err(|_| "carry intent is corrupt".to_owned())?;
        Ok(Self {
            source: wire
                .source
                .parse()
                .map_err(|_| "carry intent source is corrupt".to_owned())?,
            continues_line: wire.continues_line,
            cause: wire.cause,
            orchestration: wire.orchestration,
        })
    }
}

/// The session counters the Context projection states beside the meter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextCounters {
    /// Admitted root turns.
    pub turns: u64,
    /// Distinct committed tool activity rows.
    pub tool_calls: u64,
    /// Committed compactions.
    pub compactions: u64,
}

/// The collections a store last loaded or committed for one record. The
/// store keeps one per record it has seen and diffs against it on commit, so
/// a collection that did not change is not rewritten.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StoreBaseline {
    pub prompt_history: Vec<String>,
    pub pending_queue: Vec<QueueItem>,
}

impl StoreBaseline {
    #[must_use]
    pub fn of(session: &SessionRecord) -> Self {
        Self {
            prompt_history: session.prompt_history.clone(),
            pending_queue: session.pending_queue.clone(),
        }
    }
}

impl SessionRecord {
    /// The exact selected account; a target never supplies an absent account.
    pub fn account_ref(
        &self,
    ) -> Result<crate::account::AccountRef, crate::capabilities::AccountAdmissionError> {
        self.account
            .as_deref()
            .ok_or_else(
                || crate::capabilities::AccountAdmissionError::AccountTargetMismatch {
                    account: "<unbound>".into(),
                },
            )?
            .parse()
    }

    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        let now = now_unix_secs();
        let id = new_uuid_v4_string();
        Self {
            store_revision: 0,
            create_operation_id: format!("plak:create:{id}"),
            node: SessionNode::genesis(&id),
            id,
            workspace_root,
            created_at_unix: now,
            updated_at_unix: now,
            account: None,
            model: None,
            effort: None,
            composition: ProductCompositionSelection::FollowCatalog,
            target_selection: ProductTargetSelection::Native,
            title: None,
            source_product_session_id: None,
            source_workflow_operation_id: None,
            plakous_locator: None,
            frozen_tools: None,
            transcript: ProductTranscript::default(),
            first_prompt: None,
            prompt_history: Vec::new(),
            pending_queue: Vec::new(),
            queue_commands: QueueCommands::default(),
            queue_projection_debt: false,
            queue_drain_state: QueueDrainState::LiveAutoDrain,
            root_turn_count: 0,
            child_wake_suppressed: false,
            tool_call_count: 0,
            compaction_count: 0,
            carry_intent: None,
            root_profile: None,
            runner: None,
        }
    }

    /// Queue a prompt the operator typed in this process.
    ///
    /// A pause holds the prompts that were waiting when it began. Once the
    /// queue has emptied, nothing is paused any
    /// more, so a new live prompt starts a live batch that drains. Without
    /// this a pause left behind by a finished workflow or a restore trapped
    /// every later follow-up until the operator resumed it by hand.
    pub(crate) fn enqueue_live(
        &mut self,
        draft: impl Into<plakous_host_protocol::Draft>,
    ) -> Result<QueueItem, queue::Refusal> {
        if self.pending_queue.is_empty() {
            self.queue_drain_state = QueueDrainState::LiveAutoDrain;
        }
        queue::enqueue(&mut self.pending_queue, draft)
    }

    /// Project a loaded record for recovery without changing its durable
    /// revision or history. Outstanding work never resumes draining implicitly.
    pub fn pause_restored_work(&mut self) {
        if !self.pending_queue.is_empty()
            || self
                .pending_queue
                .iter()
                .any(|item| self.queue_commands.is_locked(&item.id))
        {
            self.queue_drain_state = QueueDrainState::RestoredPaused;
        }
    }

    /// The counters the Context projection states, read together so one
    /// projection never mixes two snapshots of the record.
    #[must_use]
    pub const fn context_counters(&self) -> ContextCounters {
        ContextCounters {
            turns: self.root_turn_count,
            tool_calls: self.tool_call_count,
            compactions: self.compaction_count,
        }
    }

    /// Records one committed compaction.
    pub fn record_compaction(&mut self) {
        self.compaction_count = self.compaction_count.saturating_add(1);
    }

    pub fn touch(&mut self) {
        self.updated_at_unix = now_unix_secs();
    }

    /// The `(session id, state key)` this session was carried forward from by
    /// `plak session import`, when it was.
    #[must_use]
    pub fn carried_forward_from(&self) -> Option<(&str, &str)> {
        let key = self
            .source_workflow_operation_id
            .as_deref()?
            .strip_prefix(CARRY_FORWARD_OPERATION_PREFIX)?;
        Some((self.source_product_session_id.as_deref()?, key))
    }

    /// Records that this session was carried forward from `source_id`, which
    /// lives under another build's `state_key`.
    pub fn link_carried_forward(&mut self, source_id: &str, state_key: &str) {
        self.source_product_session_id = Some(source_id.to_owned());
        self.source_workflow_operation_id =
            Some(format!("{CARRY_FORWARD_OPERATION_PREFIX}{state_key}"));
    }

    /// Records one admitted root turn.
    pub fn record_admitted_root_turn(&mut self) {
        self.root_turn_count = self.root_turn_count.saturating_add(1);
    }

    pub fn bind_plakous_locator(&mut self, locator: SessionLocator) -> Result<(), String> {
        if let Some(current) = self.plakous_locator
            && (current.session_id() != locator.session_id()
                || current.branch_id() != locator.branch_id())
        {
            return Err("product session cannot change its Plakous session branch".to_owned());
        }
        self.plakous_locator = Some(locator);
        self.touch();
        Ok(())
    }

    /// Append to the bounded product transcript projection. Whole oldest rows
    /// are discarded; canonical history remains owned by the Plakous locator.
    pub fn append_transcript(&mut self, row: TranscriptRow) {
        self.transcript.append(row);
    }

    /// Shows `row` and records it as an operator notice intent: a
    /// product-authored row no feed fact implies (spec 02 §2.6).
    pub fn record_notice(&mut self, row: TranscriptRow) {
        self.transcript.record_notice(row);
    }

    /// Write one tool activity row in place, or append it for a new call id.
    /// The projection bounds subject and detail, so every reader sees bounded
    /// rows.
    pub fn upsert_tool_activity(&mut self, activity: ToolItem) {
        // A call id seen for the first time is one more tool call; later state
        // changes update the row and never count again. Compact workflows and
        // unread child-result notices are host rows, not model tool calls.
        if self.transcript.upsert_tool(&activity)
            && !matches!(activity.name.as_str(), "compact" | "child_result")
        {
            self.tool_call_count = self.tool_call_count.saturating_add(1);
        }
    }

    /// Append one sealed thinking row; the projection bounds it and drops
    /// blank reasoning.
    pub fn append_thinking_activity(&mut self, activity: ThinkingRow) {
        self.transcript.append(TranscriptRow::Thinking(activity));
    }

    /// Records one durably accepted composer action. Adjacent repeats do not
    /// add another shelf row; oldest rows are evicted to both product bounds.
    pub fn record_prompt(&mut self, prompt: String) {
        // An image-only message has no text to recall.
        if prompt.is_empty() || self.prompt_history.last() == Some(&prompt) {
            return;
        }
        self.prompt_history.push(prompt);
        let mut bytes = self.prompt_history.iter().map(String::len).sum::<usize>();
        let mut remove = 0;
        while remove < self.prompt_history.len()
            && (self.prompt_history.len() - remove > MAX_PROMPT_HISTORY
                || bytes > MAX_PROMPT_HISTORY_BYTES)
        {
            bytes = bytes.saturating_sub(self.prompt_history[remove].len());
            remove += 1;
        }
        if remove > 0 {
            self.prompt_history.drain(..remove);
        }
    }

    pub fn validate_for_store(&self) -> Result<(), String> {
        self.target_selection.validate()?;
        self.node.validate()?;
        if let Some(profile) = &self.root_profile {
            profile.validate()?;
        }
        if let Some(runner) = &self.runner {
            runner.validate()?;
        }
        if self.carry_intent.is_some() && self.plakous_locator.is_some() {
            return Err("a carry intent is cleared when its locator is bound".to_owned());
        }
        let selected_account = self
            .account
            .as_deref()
            .and_then(|account| account.parse::<crate::account::AccountRef>().ok());
        match (&self.target_selection, self.account.as_deref()) {
            (ProductTargetSelection::OpenRouter { .. }, Some(_))
                if selected_account.as_ref().is_some_and(|account| {
                    account.vertical() == crate::account::Vertical::OpenRouter
                }) && self.model.as_deref().is_some_and(|model| !model.is_empty())
                    && self.composition != ProductCompositionSelection::FollowCatalog
                    && self
                        .effort
                        .as_deref()
                        .is_none_or(|effort| crate::effort::parse(effort).is_some()) => {}
            (ProductTargetSelection::OpenRouter { .. }, _) => {
                return Err(
                    "OpenRouter target requires its exact account, model, closed effort, and non-native composition"
                        .to_owned(),
                );
            }
            (ProductTargetSelection::Native | ProductTargetSelection::NativePriority, Some(_))
                if selected_account.as_ref().is_some_and(|account| {
                    account.vertical() == crate::account::Vertical::OpenRouter
                }) =>
            {
                return Err("OpenRouter account requires an exact OpenRouter target".to_owned());
            }
            (ProductTargetSelection::NativePriority, Some(_))
                if selected_account.as_ref().is_some_and(|account| {
                    account.vertical() == crate::account::Vertical::Codex
                }) => {}
            (ProductTargetSelection::NativePriority, _) => {
                return Err(
                    "priority native target requires the Codex subscription account".to_owned(),
                );
            }
            (ProductTargetSelection::Platform { provider }, _)
                if selected_account.as_ref().is_some_and(|account| {
                    account.vertical().capability().platform_provider() == Some(*provider)
                }) && self.model.as_deref().is_some_and(|model| {
                    !model.is_empty()
                        && self.composition != ProductCompositionSelection::FollowCatalog
                        && (self.composition != ProductCompositionSelection::NativePtc
                            || *provider == crate::binding::PlatformProvider::OpenAi)
                }) && self
                    .effort
                    .as_deref()
                    .and_then(crate::effort::parse)
                    .is_some() => {}
            (ProductTargetSelection::Platform { .. }, _) => {
                return Err(
                    "API target requires its exact account, model, effort, and local composition"
                        .to_owned(),
                );
            }
            (ProductTargetSelection::Native, Some(account))
                if account
                    .parse::<crate::account::AccountRef>()
                    .is_ok_and(|account| account.vertical().is_api()) =>
            {
                return Err("API account requires its exact API target".to_owned());
            }
            (ProductTargetSelection::Native, _) => {}
        }
        validate_prompt_history(&self.prompt_history)?;
        validate_pending_queue(&self.pending_queue)?;
        validate_portable_identity(&self.create_operation_id, "create operation")?;
        validate_product_lineage(self)?;

        Ok(())
    }
}

fn validate_prompt_history(history: &[String]) -> Result<(), String> {
    if history.len() > MAX_PROMPT_HISTORY {
        return Err("prompt history exceeds the product count bound".to_owned());
    }
    if history.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("prompt history contains adjacent duplicates".to_owned());
    }
    if history.iter().any(|prompt| prompt.len() > MAX_PROMPT_BYTES) {
        return Err("prompt history entry exceeds the product prompt bound".to_owned());
    }
    if history.iter().map(String::len).sum::<usize>() > MAX_PROMPT_HISTORY_BYTES {
        return Err("prompt history exceeds the product byte bound".to_owned());
    }
    Ok(())
}

#[must_use]
pub fn workspace_identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn new_uuid_v4_string() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Accept only the one lowercase, hyphenated spelling used for durable product
/// identities. Callers may use the returned value in filesystem paths because
/// it contains exactly the canonical UUID alphabet and separators.
pub fn canonical_product_session_id(value: &str) -> Result<&str, String> {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| "session has invalid product identity".to_owned())?;
    if parsed.hyphenated().to_string() != value {
        return Err("session has non-canonical product identity".to_owned());
    }
    Ok(value)
}

fn validate_pending_queue(pending_queue: &[QueueItem]) -> Result<(), String> {
    if pending_queue.len() > MAX_PENDING_PROMPTS {
        return Err("pending queue exceeds the product count bound".to_owned());
    }
    let mut queue_ids = HashSet::new();
    let mut aggregate_bytes = 0usize;
    if pending_queue
        .iter()
        .map(|item| queue::image_bytes(&item.images))
        .sum::<usize>()
        > MAX_PENDING_QUEUE_IMAGE_BYTES
    {
        return Err("pending queue exceeds the product image bound".to_owned());
    }
    for item in pending_queue {
        if queue::validate_images(&item.images).is_err() {
            return Err("session has invalid pending queue images".to_owned());
        }
        aggregate_bytes = aggregate_bytes
            .checked_add(item.text.len())
            .ok_or_else(|| "pending queue byte count overflowed".to_owned())?;
        if uuid::Uuid::parse_str(&item.id).is_err()
            || item.revision == 0
            || (item.text.is_empty() && item.images.is_empty())
            || item.text.len() > MAX_PROMPT_BYTES
            || aggregate_bytes > MAX_PENDING_QUEUE_BYTES
            || !queue_ids.insert(item.id.clone())
        {
            return Err("session has invalid pending queue state".to_owned());
        }
    }
    Ok(())
}

fn validate_product_lineage(session: &SessionRecord) -> Result<(), String> {
    canonical_product_session_id(&session.id)?;
    match (
        session.source_product_session_id.as_deref(),
        session.source_workflow_operation_id.as_deref(),
    ) {
        (None, None) => {}
        (Some(source), Some(operation))
            if canonical_product_session_id(source).is_ok() && source != session.id =>
        {
            validate_portable_identity(operation, "source workflow operation")?;
        }
        _ => return Err("session has incomplete or invalid product lineage".to_owned()),
    }
    Ok(())
}

fn validate_portable_identity(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err(format!("delivery has invalid {label} identity"));
    }
    Ok(())
}

// The identity-binding digests a durable session store commits under. They are
// pure functions over the record vocabulary above, so they live with it rather
// than with any one store implementation; the product's SQLite store and the
// in-memory double must agree on them byte for byte.

/// Length-framed SHA-256 over `parts`, lowercase hex.
///
/// The framing is what makes two adjacent fields un-ambiguous, so every
/// durable identity binding in the product store is this exact preimage.
#[must_use]
pub fn digest_parts(parts: &[&str]) -> String {
    let mut preimage = Vec::new();
    for part in parts {
        preimage.extend_from_slice(&(part.len() as u64).to_be_bytes());
        preimage.extend_from_slice(part.as_bytes());
    }
    sha256_hex(&[&preimage])
}

/// The `(reason, selection)` pair a workflow kind is journalled as.
#[must_use]
pub fn encode_workflow_kind(kind: &SessionWorkflowKind) -> (Option<&str>, Option<String>) {
    match kind {
        SessionWorkflowKind::Compact(reason) => (Some(reason.as_str()), None),
        SessionWorkflowKind::New => (None, None),
        SessionWorkflowKind::Fork => (None, None),
        SessionWorkflowKind::Rebind(v) => (None, serde_json::to_string(v).ok()),
        SessionWorkflowKind::Switch(v) => (None, v.encode().ok()),
    }
}

#[cfg(test)]
mod tests;
