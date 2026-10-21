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
pub mod projector;
pub(crate) mod queue;
mod successor;
mod transcript;
pub use journal::{ProductCompactReason, RebindSelection, ResultMailBatch, SessionWorkflowKind};
pub use plakous_host_protocol::views::transcript::{
    AutoContinue, NoticePlacement, NoticeScope, NoticeTone, ThinkingRow, ToolItem, TranscriptRow,
};
pub use queue::{QueueCommands, QueueDrainState, QueueItem};
pub(crate) use transcript::bound_row;
pub use transcript::named_image_chip;
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

pub fn clear_create_operation_id(workflow_operation_id: &str) -> Result<String, String> {
    validate_portable_identity(workflow_operation_id, "clear workflow operation")?;
    let operation = format!("{workflow_operation_id}:create");
    validate_portable_identity(&operation, "clear create operation")?;
    Ok(operation)
}

pub(crate) fn bound_subject(value: &str) -> String {
    bound_subject_to(value, 240)
}

/// A tool or process subject as the live path stores it: redact, then bound
/// to one line (spec 04 §5). Redaction is idempotent, so import runs the
/// same function rather than a second copy.
pub(crate) fn live_tool_subject(value: &str) -> String {
    bound_subject(&crate::session_log::redact_text_preview(value))
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

fn bound_activity(value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let marker = "\n[preview truncated]";
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
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        let now = now_unix_secs();
        let id = new_uuid_v4_string();
        Self {
            store_revision: 0,
            create_operation_id: format!("plak:create:{id}"),
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
        match (&self.target_selection, self.account.as_deref()) {
            (ProductTargetSelection::OpenRouter { .. }, Some("openrouter"))
                if self.model.as_deref().is_some_and(|model| !model.is_empty())
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
            (
                ProductTargetSelection::Native | ProductTargetSelection::NativePriority,
                Some("openrouter"),
            ) => {
                return Err("OpenRouter account requires an exact OpenRouter target".to_owned());
            }
            (ProductTargetSelection::NativePriority, Some("codex")) => {}
            (ProductTargetSelection::NativePriority, _) => {
                return Err(
                    "priority native target requires the Codex subscription account".to_owned(),
                );
            }
            (ProductTargetSelection::Platform { provider }, account)
                if account == Some(provider.account())
                    && self.model.as_deref().is_some_and(|model| {
                        !model.is_empty()
                            && self.composition != ProductCompositionSelection::FollowCatalog
                            && crate::platform::api_composition(*provider, model, self.composition)
                                .is_ok()
                    })
                    && self
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
                if crate::binding::PlatformProvider::for_account(account).is_some() =>
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
        SessionWorkflowKind::Clear => (None, None),
        SessionWorkflowKind::Fork => (None, None),
        SessionWorkflowKind::Rebind(v) => (None, serde_json::to_string(v).ok()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::projector::{
        Delivery, ProductIntents, SessionViews, Submission, SubmissionKind, ViewWrites,
    };
    use crate::session_store::{MemoryStore, SessionRecordStore};

    fn submission(item: &QueueItem, command: &str) -> Submission {
        Submission {
            command_id: command.into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        }
    }

    fn project_delivery(
        store: &MemoryStore,
        session: &SessionRecord,
        command: &str,
        delivery: Delivery,
    ) {
        let mut views = SessionViews::empty();
        views.state.deliveries.insert(command.into(), delivery);
        store
            .commit_projection_views(
                &session.id,
                &ViewWrites::between(&SessionViews::empty(), &views),
                &views,
            )
            .unwrap();
    }

    use super::*;
    use plakous_host_protocol::views::presentation::row_bytes;

    #[test]
    fn the_root_turn_count_advances_per_admitted_turn() {
        let mut session = SessionRecord::new(PathBuf::from("/work/exact"));
        assert_eq!(session.root_turn_count, 0);
        for expected in 1..=10 {
            session.record_admitted_root_turn();
            assert_eq!(session.root_turn_count, expected);
        }
        assert!(!session.queue_commands.has_pending());
    }

    fn running_activity(call_id: &str) -> ToolItem {
        ToolItem {
            parent_call_id: None,
            cell_id: None,
            call_id: call_id.to_owned(),
            name: "read".to_owned(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("read"),
            state: "running".to_owned(),
            subject: Some(format!("subject {call_id}")),
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            wait: None,
        }
    }

    #[test]
    fn written_subject_does_not_show_a_preview_marker() {
        let mut session = SessionRecord::new(PathBuf::from("/work"));
        let mut activity = running_activity("truncated");
        activity.subject = Some("first\n[preview truncated]".into());
        session.upsert_tool_activity(activity);
        assert_eq!(
            session
                .transcript
                .tool_activity("truncated")
                .unwrap()
                .subject
                .as_deref(),
            Some("first…")
        );
    }

    #[test]
    fn written_subject_is_single_line_and_byte_bounded() {
        let mut session = SessionRecord::new(PathBuf::from("/work"));
        let mut activity = running_activity("a");
        activity.subject = Some(format!("first\n{}", "界".repeat(200)));
        session.upsert_tool_activity(activity);
        let subject = session
            .transcript
            .tool_activity("a")
            .unwrap()
            .subject
            .unwrap();
        assert!(subject.len() <= 240);
        assert!(subject.contains('↵'));
        assert!(subject.ends_with('…'));
        assert!(!subject.contains('\n'));
    }

    #[test]
    fn blank_thinking_is_not_a_row() {
        let mut session = SessionRecord::new(PathBuf::from("/work"));
        session.append_thinking_activity(ThinkingRow {
            location_key: "0:0".into(),
            text: "  ".into(),
            elapsed_ms: None,
        });
        assert!(session.transcript.is_empty());
        session.append_thinking_activity(ThinkingRow {
            location_key: "0:0".into(),
            text: "inventory the repo".into(),
            elapsed_ms: Some(4100),
        });
        assert!(matches!(
            &session.transcript[..],
            [TranscriptRow::Thinking(row)] if row.elapsed_ms == Some(4100)
        ));
    }

    #[test]
    fn a_tool_row_update_is_a_replace_addressed_by_call_id() {
        let mut transcript = ProductTranscript::default();
        for index in 0..1_000 {
            assert!(transcript.upsert_tool(&running_activity(&format!("call-{index}"))));
        }
        transcript.mark_committed();

        for index in 0..200 {
            let mut done = running_activity(&format!("call-{index}"));
            "succeeded".clone_into(&mut done.state);
            assert!(
                !transcript.upsert_tool(&done),
                "a known call id is a replace"
            );
        }
        assert_eq!(transcript.len(), 1_000);

        assert_eq!(
            transcript.tool_activity("call-150").map(|row| row.state),
            Some("succeeded".to_owned())
        );
        assert!(transcript.tool_activity("call-absent").is_none());
    }

    #[test]
    fn a_bounded_transcript_drops_in_batches_and_keeps_call_rows_addressable() {
        let mut transcript = ProductTranscript::default();
        for index in 0..MAX_TRANSCRIPT_LINES {
            transcript.upsert_tool(&running_activity(&format!("call-{index}")));
        }
        transcript.mark_committed();
        assert_eq!(transcript.len(), MAX_TRANSCRIPT_LINES);

        transcript.append(TranscriptRow::assistant("newest"));
        let kept = MAX_TRANSCRIPT_LINES - MAX_TRANSCRIPT_LINES / 8;
        assert_eq!(transcript.len(), kept);
        assert_eq!(transcript[0], TranscriptRow::Truncated);
        assert_eq!(transcript.last(), Some(&TranscriptRow::assistant("newest")));
        let dropped = MAX_TRANSCRIPT_LINES + 1 - (kept - 1);
        assert!(transcript.tool_activity("call-0").is_none());
        let survivor = format!("call-{dropped}");
        let mut done = running_activity(&survivor);
        "succeeded".clone_into(&mut done.state);
        assert!(!transcript.upsert_tool(&done));
        assert_eq!(
            transcript.tool_activity(&survivor).map(|row| row.state),
            Some("succeeded".to_owned())
        );
        assert_eq!(transcript[1], TranscriptRow::Tool(done));

        // The next append under the bound drops nothing: batches, not churn.
        transcript.mark_committed();
        transcript.append(TranscriptRow::assistant("after"));
        assert_eq!(transcript.len(), kept + 1);
    }

    #[test]
    fn stored_rows_rebuild_the_same_projection_and_identities() {
        let mut live = ProductTranscript::default();
        live.append(TranscriptRow::user("go"));
        live.upsert_tool(&running_activity("call-a"));
        live.append(TranscriptRow::assistant("done"));
        let rows = live
            .rows()
            .into_iter()
            .map(|(row, call_id)| (row.clone(), call_id.map(str::to_owned)))
            .collect::<Vec<_>>();
        assert_eq!(rows[1].1.as_deref(), Some("call-a"));
        let restored = ProductTranscript::from_rows(rows);
        assert_eq!(restored, live);
        assert_eq!(restored.pending_notices(), []);
        assert_eq!(
            restored.tool_activity("call-a").map(|row| row.call_id),
            Some("call-a".to_owned())
        );
    }

    fn locator(checkpoint: u8) -> SessionLocator {
        format!(
            "00000000-0000-0000-0000-000000000001/\
             00000000-0000-0000-0000-000000000002/\
             00000000-0000-0000-0000-{checkpoint:012}"
        )
        .parse()
        .unwrap()
    }

    fn branch_locator(branch: u8, checkpoint: u8) -> SessionLocator {
        format!(
            "00000000-0000-0000-0000-000000000001/\
             00000000-0000-0000-0000-{branch:012}/\
             00000000-0000-0000-0000-{checkpoint:012}"
        )
        .parse()
        .unwrap()
    }

    fn rebind_selection(model: &str) -> Box<RebindSelection> {
        Box::new(RebindSelection {
            provider: "xai".into(),
            account: "grok".into(),
            model: model.into(),
            effort: Some("high".into()),
            composition: ProductCompositionSelection::FollowCatalog,
            target_selection: ProductTargetSelection::Native,
            protocol: "responses".into(),
            route: "xai:oauth:responses".into(),
            requested_model: model.into(),
            selected_configuration: "selected-config".into(),
            effective_configuration: "effective-config".into(),
            account_binding: "account-binding".into(),
        })
    }

    fn openrouter_target() -> ProductTargetSelection {
        use crate::binding::{OpenRouterRoutingSelection, ProtocolSelection, WireBinding};
        ProductTargetSelection::OpenRouter {
            wire: WireBinding::resolve(ProtocolSelection::Auto),
            routing: OpenRouterRoutingSelection::GatewayManaged,
        }
    }

    fn bound_session(workspace_root: impl Into<PathBuf>) -> SessionRecord {
        let mut session = SessionRecord::new(workspace_root.into());
        session.bind_plakous_locator(locator(3)).unwrap();
        session
    }

    #[test]
    fn product_session_identity_has_one_canonical_spelling() {
        let exact = "018f2537-5ed2-7b11-a4b1-f009be85f101";
        assert_eq!(canonical_product_session_id(exact), Ok(exact));
        for invalid in [
            "018F2537-5ED2-7B11-A4B1-F009BE85F101",
            "018f25375ed27b11a4b1f009be85f101",
            "../018f2537-5ed2-7b11-a4b1-f009be85f101",
        ] {
            assert!(canonical_product_session_id(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn prompt_history_suppresses_adjacent_duplicates_and_enforces_both_bounds() {
        let mut session = bound_session("/work/prompt-history-bounds");
        session.record_prompt("same".into());
        session.record_prompt("same".into());
        assert_eq!(session.prompt_history, ["same"]);

        for index in 0..=MAX_PROMPT_HISTORY {
            session.record_prompt(format!("prompt-{index}"));
        }
        assert_eq!(session.prompt_history.len(), MAX_PROMPT_HISTORY);
        assert_eq!(
            session.prompt_history.first().map(String::as_str),
            Some("prompt-1")
        );

        let large = "x".repeat(MAX_PROMPT_BYTES - 1);
        for index in 0..5 {
            session.record_prompt(format!("{index}{large}"));
        }
        assert!(
            session
                .prompt_history
                .iter()
                .map(String::len)
                .sum::<usize>()
                <= MAX_PROMPT_HISTORY_BYTES
        );
        assert_eq!(session.prompt_history.len(), 4);
        assert!(session.validate_for_store().is_ok());
    }

    #[test]
    fn queue_bounds_are_enforced_by_typed_store_validation() {
        let mut session = bound_session("/work/queue-bounds");
        session.pending_queue.push(QueueItem::new("ok"));
        let oversized = "x".repeat(MAX_PROMPT_BYTES + 1);
        session.pending_queue[0].text = oversized;
        assert!(session.validate_for_store().is_err());
        session.pending_queue = (0..=MAX_PENDING_PROMPTS)
            .map(|index| QueueItem::new(format!("item-{index}")))
            .collect();
        assert!(session.validate_for_store().is_err());
    }

    #[test]
    fn product_transcript_projection_is_bounded_without_touching_locator() {
        let mut session = bound_session("/work/long");
        let authority = session.plakous_locator;
        session.transcript = ProductTranscript::from_lines(
            (0..MAX_TRANSCRIPT_LINES + 50)
                .map(|index| TranscriptRow::assistant(format!("row {index}")))
                .collect(),
        );
        session.append_transcript(TranscriptRow::assistant("newest"));

        assert!(session.transcript.len() <= MAX_TRANSCRIPT_LINES);
        assert_eq!(session.transcript.first(), Some(&TranscriptRow::Truncated));
        assert_eq!(
            session.transcript.last(),
            Some(&TranscriptRow::assistant("newest"))
        );
        assert_eq!(session.plakous_locator, authority);

        let mut oversized = bound_session("/work/oversized");
        oversized.append_transcript(TranscriptRow::assistant(format!(
            "{}tail",
            "x".repeat(MAX_TRANSCRIPT_BYTES + 128)
        )));
        assert_eq!(oversized.transcript.len(), 1);
        let row = &oversized.transcript[0];
        assert!(row_bytes(row) <= MAX_TRANSCRIPT_BYTES);
        let TranscriptRow::Assistant { text } = row else {
            panic!("{row:?}");
        };
        assert!(text.starts_with(TRANSCRIPT_TRUNCATION_MARKER));
        assert!(text.ends_with("tail"));
    }

    #[test]
    fn pending_rejections_survive_settled_submissions_at_full_queue_capacity() {
        let mut session = bound_session("/work/full-queue");
        let mut intents = ProductIntents::default();
        let mut views = SessionViews::empty();
        for index in 0..MAX_PENDING_PROMPTS {
            let item = QueueItem::new("rejected");
            let request = submission(&item, &format!("rejected:{index}"));
            views.state.deliveries.insert(
                request.command_id.clone(),
                Delivery::Rejected {
                    error: "refused".into(),
                },
            );
            intents.submissions.push(request);
            session.pending_queue.push(item);
        }
        for index in 0..=MAX_PENDING_PROMPTS + 128 {
            let request = submission(&QueueItem::new("consumed"), &format!("consumed:{index}"));
            views
                .state
                .deliveries
                .insert(request.command_id.clone(), Delivery::Consumed);
            intents.submissions.push(request);
        }
        session.queue_commands = QueueCommands::from_join(&intents, Some(&views));
        assert_eq!(session.pending_queue.len(), MAX_PENDING_PROMPTS);
        for item in &session.pending_queue {
            assert!(session.queue_commands.is_rejected(item));
            assert!(!session.queue_commands.is_locked(&item.id));
            assert!(!session.queue_commands.is_consumed(item));
        }
        assert!(!session.queue_commands.has_pending());
        session.validate_for_store().unwrap();
    }

    #[test]
    fn submission_is_immutable_idempotent_and_consumes_exact_item_revision() {
        let store = MemoryStore::new();
        let mut session = bound_session("/work/submission");
        let item = QueueItem::new("exact prompt");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "plak.command.one");
        store.save_submission(&mut session, &request).unwrap();
        store.save_submission(&mut session, &request).unwrap();
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![request.clone()]
        );
        assert!(session.queue_commands.is_locked(&item.id));
        project_delivery(&store, &session, &request.command_id, Delivery::Accepted);
        assert_eq!(
            store.load(&session.id).unwrap().pending_queue,
            [] as [QueueItem; 0]
        );
        session.pending_queue[0].revision += 1;
        session.pending_queue[0].text = "new revision".into();
        store.save(&mut session).unwrap();
        assert_eq!(
            store.load(&session.id).unwrap().pending_queue,
            session.pending_queue
        );
        let conflict = Submission {
            canonical_command: "different".into(),
            ..request
        };
        assert!(store.save_submission(&mut session, &conflict).is_err());
    }

    #[test]
    fn command_binding_is_independent_of_the_projected_queue() {
        let store = MemoryStore::new();
        let mut session = bound_session("/work/locked-submission");
        let item = QueueItem::new("immutable retry");
        session.pending_queue.push(item.clone());
        let request = submission(&item, "plak.command.locked");
        store.save_submission(&mut session, &request).unwrap();
        session.pending_queue.clear();
        assert!(session.validate_for_store().is_ok());
        store.save(&mut session).unwrap();
        let changed = Submission {
            canonical_command: "mutated".into(),
            ..request.clone()
        };
        assert!(store.save_submission(&mut session, &changed).is_err());
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![request]
        );
    }

    #[test]
    fn rejected_submission_retains_item_and_pauses_restored_drain() {
        let store = MemoryStore::new();
        let mut session = bound_session("/work/rejected");
        let item = QueueItem::new("retain me");
        session.pending_queue.push(item.clone());
        let request = Submission {
            kind: SubmissionKind::SendNowSteer,
            target_turn_id: Some("00000000-0000-4000-8000-000000000009".into()),
            ..submission(&item, "plak.command.reject")
        };
        store.save_submission(&mut session, &request).unwrap();
        project_delivery(
            &store,
            &session,
            &request.command_id,
            Delivery::Rejected {
                error: "refused".into(),
            },
        );
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.pending_queue, vec![item.clone()]);
        assert_eq!(loaded.queue_drain_state, QueueDrainState::RestoredPaused);
        assert!(loaded.queue_commands.is_rejected(&item));
        assert!(!loaded.queue_commands.is_locked(&item.id));
    }

    #[test]
    fn send_now_binds_exact_target_turn_and_cannot_become_a_root() {
        let store = MemoryStore::new();
        let mut session = bound_session("/work/send-now");
        let item = QueueItem::new("steer exactly once");
        session.pending_queue.push(item.clone());
        let request = Submission {
            kind: SubmissionKind::SendNowSteer,
            target_turn_id: Some("00000000-0000-4000-8000-000000000042".into()),
            ..submission(&item, "plak.command.send-now.exact")
        };
        store.save_submission(&mut session, &request).unwrap();
        let loaded = store.load_intents(&session.id).unwrap();
        assert_eq!(loaded.submissions, vec![request.clone()]);
        for changed in [
            Submission {
                target_turn_id: Some("00000000-0000-4000-8000-000000000043".into()),
                ..request.clone()
            },
            Submission {
                kind: SubmissionKind::RootStart,
                target_turn_id: None,
                ..request.clone()
            },
        ] {
            assert!(store.save_submission(&mut session, &changed).is_err());
        }
    }

    #[test]
    fn explicit_effort_binding_is_round_tripped_on_the_session_record() {
        let mut session = bound_session("/work/effort");
        assert_eq!(session.effort, None);
        session.effort = Some("medium".into());
        assert_eq!(session.effort.as_deref(), Some("medium"));
        assert!(session.validate_for_store().is_ok());
    }

    fn successor_from_fact(
        source: &SessionRecord,
        operation: &str,
        locator: SessionLocator,
        kind: SessionWorkflowKind,
        target: SessionLocator,
    ) -> SessionRecord {
        let request = projector::WorkflowRequest {
            operation_id: operation.to_owned(),
            source_locator: locator,
            kind,
        };
        let target = plakous::sessions::feed::FeedLocator {
            session_id: target.session_id(),
            branch_id: target.branch_id(),
            checkpoint_id: target.checkpoint_id(),
        };
        source
            .successor_from_request(
                &request,
                &projector::Successor {
                    operation_id: Some(operation.to_owned()),
                    transition: plakous::sessions::feed::FeedBranchTransition::Fork,
                    target,
                    published_seq: plakous::sessions::feed::FeedSeq::new(1),
                    created_at_unix: source.updated_at_unix,
                    seed: None,
                },
            )
            .unwrap()
    }

    #[test]
    fn branch_record_preserves_presentation_but_not_pending_work() {
        let mut source = bound_session("/work/branch");
        source.target_selection = openrouter_target();
        source.account = Some("codex".into());
        source.model = Some("gpt-source".into());
        source.effort = Some("medium".into());
        source.title = Some("preserved title".into());
        source
            .transcript
            .append(TranscriptRow::user("preserved presentation"));
        source
            .pending_queue
            .push(QueueItem::new("must stay source-owned"));
        let intents = ProductIntents {
            submissions: vec![submission(&source.pending_queue[0], "plak.command.source")],
            ..ProductIntents::default()
        };
        source.queue_commands = QueueCommands::from_join(&intents, None);
        assert!(source.queue_commands.has_pending());
        let mut target = successor_from_fact(
            &source,
            "plak:rebind:branch",
            locator(3),
            SessionWorkflowKind::Rebind(rebind_selection("grok-target")),
            branch_locator(9, 10),
        );
        assert_ne!(target.id, source.id);
        assert_eq!(
            target.source_product_session_id.as_deref(),
            Some(source.id.as_str())
        );
        assert_eq!(
            target.source_workflow_operation_id.as_deref(),
            Some("plak:rebind:branch")
        );
        assert_eq!(target.title, source.title);
        assert!(target.transcript.is_empty());
        assert_eq!(target.account.as_deref(), Some("grok"));
        assert_eq!(target.model.as_deref(), Some("grok-target"));
        assert_eq!(target.effort.as_deref(), Some("high"));
        assert_eq!(target.target_selection, ProductTargetSelection::Native);
        assert_eq!(target.plakous_locator, Some(branch_locator(9, 10)));
        assert_eq!(target.pending_queue, [] as [QueueItem; 0]);
        assert!(!target.queue_commands.has_pending());

        assert!(target.bind_plakous_locator(locator(11)).is_err());
        assert!(target.validate_for_store().is_ok());
    }

    #[test]
    fn non_reasoning_rebind_preserves_absent_effort_in_target_record() {
        let mut source = bound_session("/work/non-reasoning-rebind");
        source.account = Some("codex".into());
        source.model = Some("reasoning-source".into());
        source.effort = Some("medium".into());
        let operation = "plak:rebind:non-reasoning";
        let mut selection = rebind_selection("claude-sonnet-5");
        selection.provider = "claude".into();
        selection.account = "claude".into();
        selection.effort = None;
        let target = successor_from_fact(
            &source,
            operation,
            locator(3),
            SessionWorkflowKind::Rebind(selection),
            branch_locator(8, 10),
        );
        assert_eq!(target.account.as_deref(), Some("claude"));
        assert_eq!(target.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(target.effort, None);
        assert!(target.validate_for_store().is_ok());
    }

    #[test]
    fn rebind_lineage_transplants_history_and_always_preserves_its_immediate_source() {
        let mut codex = bound_session("/work/provider-lineage");
        codex.account = Some("codex".into());
        codex.model = Some("gpt-5.6-sol".into());
        codex.effort = Some("medium".into());
        codex.append_transcript(TranscriptRow::assistant("I am an older Codex target"));
        codex.append_transcript(TranscriptRow::user("keep this exact history"));
        codex.record_prompt("keep this exact history".into());
        let source_projection = codex.transcript.clone();
        let source_prompts = codex.prompt_history.clone();

        let grok_operation = "plak:rebind:cross-provider";
        let mut grok_selection = rebind_selection("grok-code-fast-1");
        grok_selection.composition = ProductCompositionSelection::JsPtc;
        let grok = successor_from_fact(
            &codex,
            grok_operation,
            locator(3),
            SessionWorkflowKind::Rebind(grok_selection),
            branch_locator(12, 13),
        );

        assert_eq!(codex.account.as_deref(), Some("codex"));
        assert_eq!(codex.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(codex.effort.as_deref(), Some("medium"));
        assert_eq!(codex.transcript, source_projection);
        assert!(grok.transcript.is_empty());
        assert_eq!(grok.prompt_history, source_prompts);
        assert_eq!(
            grok.source_product_session_id.as_deref(),
            Some(codex.id.as_str())
        );
        assert_eq!(grok.account.as_deref(), Some("grok"));
        assert_eq!(grok.composition, ProductCompositionSelection::JsPtc);

        let fast_operation = "plak:rebind:effort-tier";
        let mut fast_selection = rebind_selection("gpt-5.6-sol");
        fast_selection.provider = "codex".into();
        fast_selection.account = "codex".into();
        fast_selection.effort = Some("high".into());
        fast_selection.composition = ProductCompositionSelection::FollowCatalog;
        fast_selection.target_selection = ProductTargetSelection::NativePriority;
        let fast = successor_from_fact(
            &grok,
            fast_operation,
            branch_locator(12, 13),
            SessionWorkflowKind::Rebind(fast_selection),
            branch_locator(14, 15),
        );

        assert_ne!(fast.id, codex.id, "returning to an old tuple still forks");
        assert_eq!(
            fast.source_product_session_id.as_deref(),
            Some(grok.id.as_str())
        );
        assert!(fast.transcript.is_empty());
        assert_eq!(fast.effort.as_deref(), Some("high"));
        assert_eq!(
            fast.target_selection,
            ProductTargetSelection::NativePriority
        );
        assert_eq!(grok.account.as_deref(), Some("grok"));
        assert_eq!(grok.composition, ProductCompositionSelection::JsPtc);
    }

    #[test]
    fn fork_preserves_exact_target_selection_without_reresolution() {
        let mut source = bound_session("/work/fork-target");
        source.target_selection = openrouter_target();
        source.account = Some("openrouter".into());
        source.model = Some("vendor/model".into());
        source.effort = Some("high".into());
        source.composition = ProductCompositionSelection::Direct;
        let operation = "plak:fork:target-selection";
        let target = successor_from_fact(
            &source,
            operation,
            locator(3),
            SessionWorkflowKind::Fork,
            branch_locator(8, 9),
        );
        assert_eq!(target.target_selection, source.target_selection);
    }

    #[test]
    fn clear_workflow_reuses_exact_create_identity_and_never_copies_source_history() {
        let mut source = bound_session("/work/clear");
        source.target_selection = openrouter_target();
        source.account = Some("openrouter".into());
        source.model = Some("gpt-target".into());
        source.effort = None;
        source.composition = ProductCompositionSelection::Direct;
        source.append_transcript(TranscriptRow::user("source-only"));
        source.record_prompt("source-only".into());
        let operation = "plak:clear:exact-operation";
        let request = projector::WorkflowRequest {
            operation_id: operation.to_owned(),
            kind: SessionWorkflowKind::Clear,
            source_locator: locator(3),
        };
        let target = source.clear_successor_from_request(&request).unwrap();
        assert_eq!(target.id, request.successor_product_id());
        assert_eq!(
            target.create_operation_id,
            "plak:clear:exact-operation:create"
        );
        assert_eq!(
            target.plakous_locator,
            Some(
                plakous::sessions::AgentSessions::genesis_locator_for(
                    &plakous::sessions::SessionOperationId::new(&target.create_operation_id)
                        .unwrap(),
                )
                .unwrap()
            )
        );
        assert_eq!(target.target_selection, source.target_selection);
        assert!(target.transcript.is_empty());
        assert_eq!(target.prompt_history, [] as [String; 0]);
        assert_eq!(target.pending_queue, [] as [QueueItem; 0]);

        assert_eq!(
            target.source_product_session_id.as_deref(),
            Some(source.id.as_str())
        );
        assert!(target.validate_for_store().is_ok());
    }

    #[test]
    fn locator_may_advance_only_within_the_bound_runtime_branch() {
        let mut session = bound_session("/work/exact");
        session.bind_plakous_locator(locator(4)).unwrap();
        assert_eq!(session.plakous_locator, Some(locator(4)));

        let other_branch = concat!(
            "00000000-0000-0000-0000-000000000001/",
            "00000000-0000-0000-0000-000000000099/",
            "00000000-0000-0000-0000-000000000005"
        )
        .parse()
        .unwrap();
        assert!(session.bind_plakous_locator(other_branch).is_err());
        assert_eq!(session.plakous_locator, Some(locator(4)));
    }

    #[test]
    fn openrouter_persisted_effort_is_closed_and_never_defaulted() {
        let mut session = bound_session("/work/openrouter-effort");
        session.account = Some("openrouter".into());
        session.model = Some("vendor/model".into());
        session.composition = ProductCompositionSelection::Direct;
        session.target_selection = openrouter_target();
        session.effort = Some("future-default".into());
        assert!(session.validate_for_store().is_err());

        session.effort = Some("high".into());
        assert!(session.validate_for_store().is_ok());
    }
}
