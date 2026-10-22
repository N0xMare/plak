//! Session carry-forward across builds with different state keys.
//!
//! A build never reads another build's stores as session authority. The one
//! thing that crosses is a portable session reference: `plak session export` writes it
//! from this build's state, `plak session import` creates a new linked session
//! from the full neutral conversation. Version 7 alone retains its checkpoint
//! and reference archive importer.

mod history;

use std::env;
use std::fmt;
use std::fs;
use std::io::IsTerminal as _;
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use plakous::model::{
    Actor, ContentBlock, ConversationItem, EntryId, HistorySegment, HistorySegmentKind, ModelId,
    Provenance, TextBlock,
};
use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, PortableSessionHistory};
use plakous_host_protocol::topology::{
    NodeDisposition, NodeStatus, WorkspaceMode, WorkspaceStatus,
};
use plakous_host_protocol::views::presentation::row_bytes;
use plakous_hosted::sessions::{MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINES, TranscriptRow};
use plakous_recursive_subagents_store::TreeInspection;
use serde::{Deserialize, Serialize};

use crate::cli_output::{TerminalMode, safe_terminal_block};
use crate::home::paths::PlakPaths;
use crate::product_config::{ProductCompositionSelection, ProductTargetSelection};
use crate::shell::services::Services;
use crate::store::sessions::{ProductTranscript, QueueDrainState, QueueItem, SessionRecord};
use crate::{CliFailure, CliFailureKind, CliResult, ProcessOutcome, commands};

/// The document's `format` value.
pub(crate) const FORMAT: &str = "plak-session-export";
/// The version this build writes. A build imports documents of this version
/// and of the immediately previous one. Any change to the document, including
/// the embedded canonical-history and binding encodings, bumps this; the
/// previous version's importer is kept for one bump only.
pub(crate) const VERSION: u32 = 8;
const PREVIOUS_VERSION: u32 = 7;
pub(crate) const IMPORTABLE_VERSIONS: [u32; 2] = [PREVIOUS_VERSION, VERSION];
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_REFERENCE_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 256 * 1024 * 1024;

/// One exported session.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryDocument {
    #[serde(skip)]
    local_digest: Option<String>,
    format: String,
    version: u32,
    source: CarrySource,
    workspace_root: PathBuf,
    title: Option<String>,
    binding: CarryBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<CarryCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive: Option<CarryArchive>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history: Option<history::History>,
    /// Unresolved source workflow facts retained as inert operator context.
    /// They are never restored into the new session's executable journal.
    pending_work: Vec<PendingWorkEntry>,
    /// Queued prompts, oldest first. A prompt already handed to the runtime is
    /// never queued here: export refuses while one is unresolved.
    queue: Vec<String>,
    prompt_history: Vec<String>,
    /// Typed product transcript rows, oldest first, shared by versions 7 and 8.
    #[serde(deserialize_with = "bounded_transcript")]
    transcript: Vec<TranscriptRow>,
    /// Non-live sub-agents the source tree held; their trees are not carried.
    source_subagents: usize,
    /// Settled source topology rows omitted from the unresolved ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_resolved_count: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryCheckpoint {
    replay_account: String,
    replay_model: ModelId,
    user_intent: String,
    reference: String,
    truncated: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryArchive {
    /// Readable, inert evidence. Import never feeds these entries to a model.
    entries: Vec<ArchiveEntry>,
    #[serde(default)]
    omitted_entries: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArchiveEntry {
    actor: String,
    text: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingWorkEntry {
    operation: String,
    kind: String,
    state: String,
    detail: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CarrySource {
    session_id: String,
    state_key: String,
    producer: ProducerBuild,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ProducerBuild {
    Verified { binary_id: String },
    Unavailable,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CarryBinding {
    account: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    composition: ProductCompositionSelection,
    target_selection: ProductTargetSelection,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ImportError {
    Oversized {
        section: &'static str,
        bytes: usize,
        maximum: usize,
    },
    Invalid(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversized {
                section,
                bytes,
                maximum,
            } => write!(
                out,
                "session export {section} is oversized ({bytes} bytes; maximum {maximum})"
            ),
            Self::Invalid(message) => out.write_str(message),
        }
    }
}
impl std::error::Error for ImportError {}
impl From<String> for ImportError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

/// A document's transcript rows, refused while they are read once they pass
/// the product transcript bounds (rows and encoded bytes): no session can
/// hold more, so a larger document is hostile and is not parsed to its end.
fn bounded_transcript<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<TranscriptRow>, D::Error> {
    struct Rows;
    impl<'de> serde::de::Visitor<'de> for Rows {
        type Value = Vec<TranscriptRow>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded sequence of transcript rows")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut rows: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error as _;
            let mut read = Vec::new();
            let mut bytes = 0_usize;
            while let Some(row) = rows.next_element::<TranscriptRow>()? {
                if read.len() == MAX_TRANSCRIPT_LINES {
                    return Err(A::Error::custom(format!(
                        "the transcript holds more than {MAX_TRANSCRIPT_LINES} rows"
                    )));
                }
                bytes = bytes.saturating_add(row_bytes(&row));
                if bytes > MAX_TRANSCRIPT_BYTES {
                    return Err(A::Error::custom(format!(
                        "the transcript exceeds {MAX_TRANSCRIPT_BYTES} bytes"
                    )));
                }
                read.push(row);
            }
            Ok(read)
        }
    }
    deserializer.deserialize_seq(Rows)
}

fn validate_size(section: &'static str, bytes: usize, maximum: usize) -> Result<(), ImportError> {
    if bytes > maximum {
        return Err(ImportError::Oversized {
            section,
            bytes,
            maximum,
        });
    }
    Ok(())
}

impl CarryDocument {
    fn validate_archive(&self) -> Result<(), ImportError> {
        let Some(archive) = &self.archive else {
            return Ok(());
        };
        validate_size(
            "archive",
            archive
                .entries
                .iter()
                .map(|e| e.actor.len().saturating_add(e.text.len()))
                .fold(0_usize, usize::saturating_add),
            MAX_ARCHIVE_BYTES,
        )
    }
}

impl CarryDocument {
    fn validate(&self) -> Result<(), ImportError> {
        if self
            .pending_work
            .iter()
            .any(|entry| entry.kind == "source_resolved_count")
        {
            return Err(
                "pending work cannot contain source_resolved_count; use the typed field"
                    .to_owned()
                    .into(),
            );
        }
        match (self.version, &self.history, &self.checkpoint, &self.archive) {
            (VERSION, Some(history), None, None) => history.validate_size(),
            (PREVIOUS_VERSION, None, Some(checkpoint), Some(_)) => {
                validate_size(
                    "checkpoint",
                    checkpoint
                        .user_intent
                        .len()
                        .saturating_add(checkpoint.reference.len()),
                    MAX_CHECKPOINT_BYTES,
                )?;
                validate_size(
                    "checkpoint user intent",
                    checkpoint.user_intent.len(),
                    MAX_CHECKPOINT_BYTES / 2,
                )?;
                validate_size(
                    "checkpoint reference",
                    checkpoint.reference.len(),
                    MAX_CHECKPOINT_BYTES / 2,
                )?;
                self.validate_archive()
            }
            _ => Err(ImportError::Invalid(
                "export payload does not match its version".to_owned(),
            )),
        }
    }

    /// The session id and state key this document was exported from.
    pub(crate) fn source(&self) -> (&str, &str) {
        (&self.source.session_id, &self.source.state_key)
    }

    /// The workspace the exported session belonged to.
    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// The new record this document becomes, the conversation its runtime is
    /// created with, and the note that tells the operator so. The record has a
    /// new id and links back to the source session and its state key.
    #[cfg(test)]
    pub(crate) fn into_import(self) -> Result<CarriedImport, ImportError> {
        self.into_import_with_replay(false)
    }

    pub(crate) fn into_import_with_replay(
        self,
        local_handoff: bool,
    ) -> Result<CarriedImport, ImportError> {
        self.validate()?;
        let mut record = SessionRecord::new(self.workspace_root.clone());
        let note = self.import_note(&record.id);
        let history = if let Some(history) = self.history {
            history.import()?
        } else {
            let checkpoint = self
                .checkpoint
                .ok_or_else(|| ImportError::Invalid("missing v7 checkpoint".to_owned()))?;
            let intent =
                checkpoint_item(&checkpoint.user_intent, Actor::User, Provenance::UserInput)?;
            let mut segments = vec![
                HistorySegment::new(HistorySegmentKind::RecentIntent, vec![intent])
                    .map_err(|e| e.to_string())?,
            ];
            if !checkpoint.reference.is_empty() {
                segments.push(
                    HistorySegment::new(
                        HistorySegmentKind::Conversation,
                        vec![checkpoint_item(
                            &checkpoint.reference,
                            Actor::Assistant,
                            Provenance::Imported,
                        )?],
                    )
                    .map_err(|e| e.to_string())?,
                );
            }
            PortableSessionHistory::new(
                segments,
                checkpoint.replay_account,
                checkpoint.replay_model,
            )
        };
        let history = if local_handoff {
            history.with_replay_validation()
        } else {
            history
                .without_replay()
                .map_err(|e| ImportError::Invalid(e.to_string()))?
        };
        apply_record_fields(
            &mut record,
            self.binding,
            self.title,
            self.prompt_history,
            self.transcript,
            self.queue,
            &self.source.session_id,
            &self.source.state_key,
        );
        Ok(CarriedImport {
            record,
            history,
            note,
            archive: self.archive,
        })
    }

    pub(crate) fn reference_text(
        &self,
        search: Option<&str>,
        tail: usize,
        destination: TerminalMode,
    ) -> String {
        match &self.archive {
            Some(archive) => render_reference_with_omissions(
                &archive.entries,
                archive.omitted_entries,
                search,
                tail,
                destination,
            ),
            None => {
                "Version 8 carries conversation history, not a separate text archive.\n".to_owned()
            }
        }
    }

    pub(crate) fn message_count(&self) -> usize {
        self.history.as_ref().map_or_else(
            || {
                self.checkpoint
                    .as_ref()
                    .map_or(0, |c| 1 + usize::from(!c.reference.is_empty()))
            },
            history::History::message_count,
        )
    }
    pub(crate) fn estimated_tokens(&self) -> u64 {
        self.history.as_ref().map_or_else(
            || {
                self.checkpoint.as_ref().map_or(0, |c| {
                    (c.user_intent.len() + c.reference.len()).div_ceil(4) as u64
                })
            },
            history::History::estimated_tokens,
        )
    }

    pub(crate) fn carry_summary(&self) -> String {
        self.history.as_ref().map_or_else(
            || "version 7 restart checkpoint (conversation unavailable)".to_owned(),
            history::History::summary,
        )
    }

    fn import_note(&self, new_id: &str) -> String {
        let carried = self.history.as_ref().map_or_else(
            || "resumed from a bounded checkpoint, with the readable archive retained only as untrusted reference data".to_owned(),
            |history| format!("conversation carried; {}",history.summary()),
        );
        import_note(&self.source, self.source_subagents, new_id, &carried)
    }
}

fn checkpoint_item(
    text: &str,
    actor: Actor,
    provenance: Provenance,
) -> Result<ConversationItem, String> {
    ConversationItem::new(
        EntryId::try_from(uuid::Uuid::new_v4())
            .map_err(|error| format!("could not identify the restart checkpoint: {error}"))?,
        actor,
        vec![ContentBlock::Text(
            TextBlock::new(text.to_owned(), None)
                .map_err(|error| format!("the restart checkpoint is invalid: {error}"))?,
        )],
        provenance,
    )
    .map_err(|error| format!("the restart checkpoint is invalid: {error}"))
}

fn apply_record_fields(
    record: &mut SessionRecord,
    binding: CarryBinding,
    title: Option<String>,
    prompt_history: Vec<String>,
    transcript: Vec<TranscriptRow>,
    queue: Vec<String>,
    source_session_id: &str,
    source_state_key: &str,
) {
    record.account = binding.account;
    record.model = binding.model;
    record.effort = binding.effort;
    record.composition = binding.composition;
    record.target_selection = binding.target_selection;
    record.title = title;
    record.prompt_history = prompt_history;
    record.transcript = ProductTranscript::from_lines(transcript);
    record.pending_queue = queue.into_iter().map(QueueItem::new).collect();
    if !record.pending_queue.is_empty() {
        record.queue_drain_state = QueueDrainState::RestoredPaused;
    }
    record.link_carried_forward(source_session_id, source_state_key);
}

fn import_note(
    source: &CarrySource,
    source_subagents: usize,
    new_id: &str,
    carried: &str,
) -> String {
    let producer = match &source.producer {
        ProducerBuild::Verified { binary_id } => format!("producer binary {binary_id}"),
        ProducerBuild::Unavailable => "producer build unavailable".to_owned(),
    };
    let mut note = format!(
        "session {new_id} linked from {} ({}, {producer}); {carried}",
        source.session_id, source.state_key,
    );
    if source_subagents > 0 {
        note.push_str(&format!(
            "; {source_subagents} non-live source sub-agent(s), their trees, and runtime state were not restored"
        ));
    }
    note
}

/// A document turned into the parts an import creates.
pub(crate) struct CarriedImport {
    pub(crate) record: SessionRecord,
    pub(crate) history: PortableSessionHistory,
    pub(crate) note: String,
    pub(crate) archive: Option<CarryArchive>,
}

/// Why a session cannot be exported now. Each names its cause; none is
/// transient enough to retry blindly.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ExportRefusal {
    /// The record never bound a canonical runtime.
    NoRuntime,
    /// A command submission or workflow request has no feed fact yet: it may
    /// still reach the runtime, and an open settles or withdraws it.
    UnsettledIntent,
    /// A sub-agent is live or sealing: it may still change the tree.
    SubagentRunning {
        agent_id: String,
        status: NodeStatus,
    },
    /// The agent tree could not be read.
    TreeUnreadable(String),
    /// A turn is running or parked with an unresolved operation.
    TurnUnfinished,
    /// A job row has no durable terminal evidence.
    BackgroundProcesses,
    /// The runtime advanced past the product record; resuming once settles it.
    RecordBehindHead,
    /// The runtime store could not be read.
    Runtime(String),
}

impl fmt::Display for ExportRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRuntime => formatter.write_str("the session has no canonical runtime yet"),
            Self::UnsettledIntent => formatter.write_str(
                "a command submission or workflow request has no feed fact yet; open the session once to settle it",
            ),
            Self::SubagentRunning { agent_id, status } => write!(
                formatter,
                "sub-agent {agent_id} is not settled ({}); let it finish or close it first", node_status_name(*status)
            ),
            Self::TreeUnreadable(error) => {
                write!(formatter, "the agent tree could not be read: {error}")
            }
            Self::TurnUnfinished => formatter.write_str(
                "a turn is running or parked with an unresolved operation; let it finish or run `plak session recover` first",
            ),
            Self::BackgroundProcesses => formatter.write_str(
                "background process outcomes are not settled; resume the session and send a prompt to deliver their notices before exporting",
            ),
            Self::RecordBehindHead => formatter.write_str(
                "the runtime advanced past the session record; resume the session once first",
            ),
            Self::Runtime(error) => write!(formatter, "the runtime store could not be read: {error}"),
        }
    }
}

/// Exports `record` from this build's state, read-only.
///
/// First applies the session's projector to its feed tail, a lease-free
/// product write (spec 02 §2.5.1), then exports the reloaded record at the
/// head its views name.
pub(crate) fn export_session(
    paths: &PlakPaths,
    record: &SessionRecord,
) -> Result<CarryDocument, ExportRefusal> {
    if record.plakous_locator.is_none() {
        return Err(ExportRefusal::NoRuntime);
    }
    let runtime = |error: String| ExportRefusal::Runtime(error);
    let records: std::sync::Arc<dyn crate::store::session_records::SessionRecordStore> =
        std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        ));
    let sessions = plakous::sessions::SessionBackend::sqlite(paths.runtime_database())
        .and_then(AgentSessions::new)
        .map_err(|error| runtime(error.to_string()))?;
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|_| runtime("application executor unavailable".to_owned()))?;
    let projected = executor
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &records, record,
        ))
        .map_err(|_| runtime("application execution failed".to_owned()))?
        .map_err(runtime)?
        .ok_or(ExportRefusal::NoRuntime)?;
    let record = records.load(&record.id).map_err(runtime)?;
    export_projected(paths, &record, &projected)
}

/// Exports `record`, whose views a projector pass just brought to `projected`.
fn export_projected(
    paths: &PlakPaths,
    record: &SessionRecord,
    projected: &plakous_hosted::runtime::ProjectedSession,
) -> Result<CarryDocument, ExportRefusal> {
    if !projected.pending_commands.is_empty() || !projected.pending_workflows.is_empty() {
        return Err(ExportRefusal::UnsettledIntent);
    }
    let locator = projected.head;
    let (source_subagents, source_resolved_count, pending_work) =
        tree_reference_facts(paths, record)?;
    match AgentSessions::inspect_recovery_sqlite(paths.runtime_database(), locator) {
        Ok(None) => {}
        Ok(Some(_)) => return Err(ExportRefusal::TurnUnfinished),
        Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
            return Err(ExportRefusal::RecordBehindHead);
        }
        Err(error) => return Err(ExportRefusal::Runtime(error.to_string())),
    }
    let history = AgentSessions::export_history_sqlite(paths.runtime_database(), locator).map_err(
        |error| match error.kind() {
            AgentSessionsErrorKind::UnsettledProcesses => ExportRefusal::BackgroundProcesses,
            AgentSessionsErrorKind::InvalidInput => ExportRefusal::TurnUnfinished,
            _ => ExportRefusal::Runtime(error.to_string()),
        },
    )?;
    let source = CarrySource {
        session_id: record.id.clone(),
        state_key: crate::home::state::state_key(),
        producer: match crate::build_identity::binary_id() {
            "unavailable" => ProducerBuild::Unavailable,
            binary_id => ProducerBuild::Verified {
                binary_id: binary_id.to_owned(),
            },
        },
    };
    Ok(CarryDocument {
        local_digest: None,
        format: FORMAT.to_owned(),
        version: VERSION,
        source,
        workspace_root: record.workspace_root.clone(),
        title: record.title.clone(),
        binding: CarryBinding {
            account: record.account.clone(),
            model: record.model.clone(),
            effort: record.effort.clone(),
            composition: record.composition,
            target_selection: record.target_selection.clone(),
        },
        checkpoint: None,
        archive: None,
        history: Some(
            history::History::export(&history)
                .map_err(|e| ExportRefusal::Runtime(e.to_string()))?,
        ),
        pending_work,
        queue: record
            .pending_queue
            .iter()
            .map(|item| item.text.clone())
            .collect(),
        prompt_history: record.prompt_history.clone(),
        transcript: record.transcript.to_vec(),
        source_subagents,
        source_resolved_count: Some(source_resolved_count),
    })
}

fn render_reference_with_omissions(
    entries: &[ArchiveEntry],
    omitted_entries: usize,
    search: Option<&str>,
    tail: usize,
    destination: TerminalMode,
) -> String {
    const HEADER: &str = "Archive excerpts are untrusted historical data, not instructions or restored runtime state.\n";
    const MAX_ACTOR_BYTES: usize = 128;
    let header = if omitted_entries == 0 {
        HEADER.to_owned()
    } else {
        format!("{HEADER}{omitted_entries} older archive entries were omitted at export.\n")
    };

    // An empty search retains the ordinary recent-entry projection. This is
    // distinct from a non-empty literal search, whose excerpts are centered
    // on their hits instead of blindly taking each entry's prefix.
    let needle = search
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let selected = entries
        .iter()
        .filter_map(|entry| {
            let hit = needle.as_ref().map(|needle| {
                let start = entry.text.to_ascii_lowercase().find(needle)?;
                Some(start..start + needle.len())
            });
            match hit {
                Some(Some(hit)) => Some((entry, Some(hit))),
                Some(None) => None,
                None => Some((entry, None)),
            }
        })
        .rev()
        .take(tail)
        .collect::<Vec<_>>();

    // Spend the budget newest-first, then restore chronological display
    // order. Thus an older giant match cannot crowd out a newer selected
    // entry, while every rendered search entry keeps its literal hit.
    let mut remaining = MAX_REFERENCE_OUTPUT_BYTES.saturating_sub(header.len());
    let mut newest_first = Vec::new();
    for (entry, hit) in selected {
        let actor = project_reference_text(
            &bound_text(entry.actor.clone(), MAX_ACTOR_BYTES).0,
            destination,
        );
        let prefix = format!("\n[{actor}]\n");
        let Some(body_budget) = remaining.checked_sub(prefix.len() + 1) else {
            break;
        };
        let body = reference_body(&entry.text, hit, body_budget, destination);
        let chunk = format!("{prefix}{body}\n");
        remaining -= chunk.len();
        newest_first.push(chunk);
    }
    let mut output = header;
    for chunk in newest_first.into_iter().rev() {
        output.push_str(&chunk);
    }
    output
}

fn reference_body(
    text: &str,
    hit: Option<std::ops::Range<usize>>,
    maximum: usize,
    destination: TerminalMode,
) -> String {
    if destination == TerminalMode::Piped {
        return match hit {
            Some(hit) => excerpt_around_hit(text, hit, maximum),
            None => bound_text(text.to_owned(), maximum).0,
        };
    }

    let Some(hit) = hit else {
        let raw = bound_text(text.to_owned(), maximum).0;
        return bound_text(safe_terminal_block(&raw), maximum).0;
    };

    // A terminal control can expand from one raw byte to the three-byte
    // replacement character. Find the largest raw excerpt whose safe
    // projection fits, instead of truncating that projection after an older
    // control-heavy prefix and potentially erasing the selected hit.
    let mut lower = hit.len().min(maximum);
    let mut upper = maximum;
    let minimal = excerpt_around_hit(text, hit.clone(), lower);
    let projected_minimal = safe_terminal_block(&minimal);
    if projected_minimal.len() > maximum || lower < hit.len() {
        return bound_text(projected_minimal, maximum).0;
    }
    let mut best = projected_minimal;
    while lower < upper {
        let middle = lower + (upper - lower).div_ceil(2);
        let candidate = safe_terminal_block(&excerpt_around_hit(text, hit.clone(), middle));
        if candidate.len() <= maximum {
            lower = middle;
            best = candidate;
        } else {
            upper = middle - 1;
        }
    }
    best
}

fn project_reference_text(value: &str, destination: TerminalMode) -> String {
    match destination {
        TerminalMode::Interactive => safe_terminal_block(value),
        TerminalMode::Piped => value.to_owned(),
    }
}

/// Returns a UTF-8-safe bounded window around one literal match. The match is
/// retained whole whenever it itself fits in `maximum`.
fn excerpt_around_hit(text: &str, hit: std::ops::Range<usize>, maximum: usize) -> String {
    const BEFORE: &str = "…\n";
    const AFTER: &str = "\n…";

    if text.len() <= maximum {
        return text.to_owned();
    }
    if maximum == 0 {
        return String::new();
    }
    if hit.len() > maximum {
        return bound_text(text[hit.start..].to_owned(), maximum).0;
    }

    let size = |start: usize, end: usize| {
        end - start
            + if start > 0 { BEFORE.len() } else { 0 }
            + if end < text.len() { AFTER.len() } else { 0 }
    };
    let mut start = hit.start;
    let mut end = hit.end;
    if size(start, end) > maximum {
        return text[hit].to_owned();
    }

    loop {
        let previous = text[..start]
            .char_indices()
            .next_back()
            .map(|(index, _)| index);
        let next = text[end..]
            .chars()
            .next()
            .map(|character| end + character.len_utf8());
        let prefer_previous = hit.start - start <= end - hit.end;
        let candidates = if prefer_previous {
            [(previous, Some(end)), (Some(start), next)]
        } else {
            [(Some(start), next), (previous, Some(end))]
        };
        let mut grew = false;
        for (candidate_start, candidate_end) in candidates {
            let (Some(candidate_start), Some(candidate_end)) = (candidate_start, candidate_end)
            else {
                continue;
            };
            if candidate_start == start && candidate_end == end {
                continue;
            }
            if size(candidate_start, candidate_end) <= maximum {
                start = candidate_start;
                end = candidate_end;
                grew = true;
                break;
            }
        }
        if !grew {
            break;
        }
    }

    let mut excerpt = String::with_capacity(size(start, end));
    if start > 0 {
        excerpt.push_str(BEFORE);
    }
    excerpt.push_str(&text[start..end]);
    if end < text.len() {
        excerpt.push_str(AFTER);
    }
    excerpt
}

/// One `- ` line per entry, or `none`, so an empty section never reads as an
/// empty list item.
fn bound_text(mut text: String, maximum: usize) -> (String, bool) {
    const MARKER: &str = "\n[bounded checkpoint/archive output truncated]\n";
    if text.len() <= maximum {
        return (text, false);
    }
    if maximum < MARKER.len() {
        let mut end = maximum;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        return (text, true);
    }
    let mut end = maximum.saturating_sub(MARKER.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(MARKER);
    (text, true)
}

/// Captures inert facts needed to find unfinished source work without
/// recreating its topology or transferring live authority. Live or sealing
/// children still refuse export; interrupted/recovery-required rows are safe
/// to reference because export is read-only and import never opens them.
fn tree_reference_facts(
    paths: &PlakPaths,
    record: &SessionRecord,
) -> Result<(usize, usize, Vec<PendingWorkEntry>), ExportRefusal> {
    let (_, observed) = crate::topology_product::inspect_session_tree(paths, record)
        .map_err(|error| ExportRefusal::TreeUnreadable(error.message().to_owned()))?;
    let TreeInspection::Tree(tree) = observed else {
        return Ok((0, 0, Vec::new()));
    };
    let tree = tree.rows();
    let child_count = count_archivable_children(
        tree.nodes()
            .values()
            .filter(|node| node.parent_agent_id.is_some())
            .map(|node| (node.agent_id.as_str(), node.status)),
    )?;
    let mut facts = tree
        .nodes()
        .values()
        .filter(|node| node.parent_agent_id.is_some() && source_node_unresolved(node.status))
        .map(|node| PendingWorkEntry {
            operation: node.agent_id.clone(),
            kind: "source_child".to_owned(),
            state: node_status_name(node.status).to_ascii_lowercase(),
            detail: Some(format!(
                "workspace={}; locator={}",
                node.workspace_id,
                if node.session_locator.is_some() {
                    "recorded"
                } else {
                    "unavailable"
                }
            )),
        })
        .collect::<Vec<_>>();
    facts.extend(
        tree.candidates()
            .values()
            .filter(|candidate| {
                candidate.adopted_by.is_none()
                    && tree
                        .workspaces()
                        .get(&candidate.workspace_id)
                        .is_some_and(|workspace| {
                            matches!(
                                workspace.status,
                                WorkspaceStatus::Active | WorkspaceStatus::Adopted
                            )
                        })
            })
            .map(|candidate| PendingWorkEntry {
                operation: candidate.candidate_id.to_string(),
                kind: "source_candidate".to_owned(),
                state: "unadopted".to_owned(),
                detail: Some(format!(
                    "workspace={}; producer={}; commit={}",
                    candidate.workspace_id, candidate.producing_agent_id, candidate.commit
                )),
            }),
    );
    facts.extend(
        tree.workspaces()
            .values()
            .filter(|workspace| {
                workspace.status.is_in_flight()
                    || (matches!(
                        workspace.status,
                        WorkspaceStatus::Active | WorkspaceStatus::Adopted
                    ) && plakous_hosted::workspace_maintenance::dirty(&workspace.path)
                        .unwrap_or(true))
            })
            .map(|workspace| PendingWorkEntry {
                operation: workspace.workspace_id.clone(),
                kind: "source_workspace".to_owned(),
                state: workspace_status_name(workspace.status).to_ascii_lowercase(),
                detail: Some(format!(
                    "owner={}; mode={}; generation={}; path={}",
                    workspace.owner_agent_id,
                    workspace_mode_name(workspace.mode),
                    workspace.generation,
                    workspace.path.display()
                )),
            }),
    );
    let resolved = child_count + tree.candidates().len() + tree.workspaces().len() - facts.len();
    Ok((child_count, resolved, facts))
}

fn source_node_unresolved(status: NodeStatus) -> bool {
    !status.is_finished()
        || matches!(
            status,
            NodeStatus::Interrupted | NodeStatus::RecoveryRequired
        )
}

const fn node_status_name(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "Pending",
        NodeStatus::Running => "Running",
        NodeStatus::Paused => "Paused",
        NodeStatus::Sealing => "Sealing",
        NodeStatus::Completed => "Completed",
        NodeStatus::Interrupted => "Interrupted",
        NodeStatus::Failed => "Failed",
        NodeStatus::Closed => "Closed",
        NodeStatus::RecoveryRequired => "RecoveryRequired",
    }
}
const fn workspace_status_name(status: WorkspaceStatus) -> &'static str {
    match status {
        WorkspaceStatus::Active => "Active",
        WorkspaceStatus::Adopting => "Adopting",
        WorkspaceStatus::Discarding => "Discarding",
        WorkspaceStatus::CleanRemoved => "CleanRemoved",
        WorkspaceStatus::Adopted => "Adopted",
        WorkspaceStatus::Discarded => "Discarded",
    }
}
const fn workspace_mode_name(mode: WorkspaceMode) -> &'static str {
    match mode {
        WorkspaceMode::Inherit => "Inherit",
        WorkspaceMode::Snapshot => "Snapshot",
        WorkspaceMode::Fork => "Fork",
    }
}

fn count_archivable_children<'a>(
    children: impl Iterator<Item = (&'a str, NodeStatus)>,
) -> Result<usize, ExportRefusal> {
    let mut count = 0;
    for (agent_id, status) in children {
        match status.disposition() {
            NodeDisposition::Idle | NodeDisposition::Dead | NodeDisposition::Interrupted => {
                count += 1;
            }
            NodeDisposition::Live | NodeDisposition::Sealing => {
                return Err(ExportRefusal::SubagentRunning {
                    agent_id: agent_id.to_owned(),
                    status,
                });
            }
        }
    }
    Ok(count)
}

/// Writes `document` to a new file only the owner can read. An existing file
/// is refused rather than overwritten.
pub(crate) fn write_document(path: &Path, document: &CarryDocument) -> Result<(), String> {
    write_document_with_directory_sync(path, document, |parent| {
        fs::File::open(parent).and_then(|dir| dir.sync_all())
    })
}

fn write_document_with_directory_sync(
    path: &Path,
    document: &CarryDocument,
    sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(document)
        .map_err(|error| format!("the export document could not be encoded: {error}"))?;
    validate_size("document", bytes.len(), MAX_DOCUMENT_BYTES).map_err(|e| e.to_string())?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("could not create export temporary file: {error}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    file.persist_noclobber(path)
        .map_err(|error| format!("could not publish {}: {error}", path.display()))?;
    // Publication already succeeded; reporting a refusal would hide the kept export.
    if let Err(error) = sync_directory(parent) {
        tracing::warn!(path = %path.display(), %error, "export published but directory sync failed");
    }
    Ok(())
}

/// Retains an archive after a reload spends its transient carry file.
/// The session id is parsed before it becomes a path component.
pub(crate) fn write_retained_archive(
    paths: &PlakPaths,
    session_id: &str,
    archive: &CarryArchive,
) -> Result<(), String> {
    let id = uuid::Uuid::parse_str(session_id)
        .map_err(|_| "session reference requires a canonical session id".to_owned())?;
    if id.to_string() != session_id {
        return Err("session reference requires a canonical session id".to_owned());
    }
    let dir = private_dir(&paths.home().join("references"))?;
    let path = dir.join(format!("{session_id}.json"));
    let bytes = serde_json::to_vec_pretty(archive)
        .map_err(|error| format!("the session archive could not be encoded: {error}"))?;
    write_private_new(&path, &bytes)
}

pub(crate) fn remove_retained_archive(paths: &PlakPaths, session_id: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    let id = uuid::Uuid::parse_str(session_id)
        .map_err(|_| "session reference requires a canonical session id".to_owned())?;
    if id.to_string() != session_id {
        return Err("session reference requires a canonical session id".to_owned());
    }
    let path = paths
        .home()
        .join("references")
        .join(format!("{session_id}.json"));
    // Reference cleanup is best-effort; it must never strand a deletion.
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return Ok(());
    };
    if !metadata.is_file() || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Ok(());
    }
    let _ = fs::remove_file(&path);
    Ok(())
}

pub(crate) fn read_retained_archive(
    paths: &PlakPaths,
    session_id: &str,
    search: Option<&str>,
    tail: usize,
    destination: TerminalMode,
) -> Result<String, String> {
    let id = uuid::Uuid::parse_str(session_id)
        .map_err(|_| "session reference requires a session id or export path".to_owned())?;
    if id.to_string() != session_id {
        return Err("session reference requires a canonical session id".to_owned());
    }
    let path = paths
        .home()
        .join("references")
        .join(format!("{session_id}.json"));
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "could not inspect retained reference {}: {error}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if !metadata.file_type().is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(format!(
                "retained reference {} is not a private owner-only regular file",
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    if !metadata.file_type().is_file() {
        return Err(format!(
            "retained reference {} is not a regular file",
            path.display()
        ));
    }
    let bytes = fs::read(&path).map_err(|error| {
        format!(
            "could not read retained reference {}: {error}",
            path.display()
        )
    })?;
    let archive: CarryArchive = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "{} is not a valid retained archive: {error}",
            path.display()
        )
    })?;
    Ok(render_reference_with_omissions(
        &archive.entries,
        archive.omitted_entries,
        search,
        tail,
        destination,
    ))
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn private_dir(dir: &Path) -> Result<PathBuf, String> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("could not create {}: {error}", dir.display())),
    }
    let metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("could not inspect {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(format!("{} is not a private directory", dir.display()));
        }
    }
    #[cfg(not(unix))]
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    Ok(dir.to_path_buf())
}

/// Reads one export document, refusing any other format or version.
pub(crate) fn read_document(path: &Path) -> Result<CarryDocument, ImportError> {
    let file =
        fs::File::open(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    validate_size(
        "document",
        file.metadata()
            .map_err(|e| e.to_string())?
            .len()
            .try_into()
            .unwrap_or(usize::MAX),
        MAX_DOCUMENT_BYTES,
    )?;
    let mut bytes = Vec::new();
    file.take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    validate_size("document", bytes.len(), MAX_DOCUMENT_BYTES)?;
    let header: DocumentHeader<'_> = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not a session export: {error}", path.display()))?;
    if header.format != FORMAT {
        return Err(ImportError::Invalid(format!(
            "{} is not a session export (format {:?})",
            path.display(),
            header.format
        )));
    }
    if !matches!(header.version, VERSION | PREVIOUS_VERSION) {
        return Err(ImportError::Invalid(format!(
            "{} is a version {} session export; this build imports versions {PREVIOUS_VERSION} and {VERSION}",
            path.display(),
            header.version
        )));
    }
    if (header.version == VERSION
        && (header.raw_history.is_none() || header.checkpoint_present || header.archive_present))
        || (header.version == PREVIOUS_VERSION
            && (header.raw_history.is_some()
                || !header.checkpoint_present
                || !header.archive_present))
    {
        return Err(ImportError::Invalid(
            "export payload fields do not match its version".to_owned(),
        ));
    }
    let invalid = |error: serde_json::Error| {
        ImportError::from(format!(
            "{} is not a valid session export: {error}",
            path.display()
        ))
    };
    if let Some(history) = header.raw_history {
        validate_size("history", history.get().len(), 128 * 1024 * 1024)?;
    }
    let mut document = serde_json::from_slice::<CarryDocument>(&bytes).map_err(invalid)?;
    document
        .transcript
        .iter_mut()
        .for_each(crate::store::normalize_exposed_name);
    document.validate()?;
    document.local_digest = Some(plakous::model::sha256_hex(&[&bytes]));
    Ok(document)
}

/// The fields every version keeps, read before the version-specific body.
#[derive(Deserialize)]
struct DocumentHeader<'a> {
    #[serde(
        borrow,
        default,
        rename = "history",
        deserialize_with = "raw_history_field"
    )]
    raw_history: Option<&'a serde_json::value::RawValue>,
    format: String,
    version: u32,

    #[serde(default, rename = "checkpoint", deserialize_with = "field_present")]
    checkpoint_present: bool,
    #[serde(default, rename = "archive", deserialize_with = "field_present")]
    archive_present: bool,
}
fn field_present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

/// The private directory carry files live in, created on first use. It sits
/// beside `state/`, so no build mistakes it for another build's state.
pub(crate) fn carry_dir(paths: &PlakPaths) -> Result<PathBuf, String> {
    private_dir(&paths.home().join("carry"))
}

/// A fresh carry file path for `session_id`; the file does not exist yet.
pub(crate) fn new_carry_file(paths: &PlakPaths, session_id: &str) -> Result<PathBuf, String> {
    Ok(carry_dir(paths)?.join(format!("{session_id}.{}.json", uuid::Uuid::new_v4())))
}

/// What `/reload` does for the session it is leaving.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ReloadPlan {
    /// The shell never committed a session: start the new build fresh.
    Fresh,
    /// Same state key: the new build opens the session as it is.
    Resume,
    /// Another state key: export here, import there.
    Carry,
    /// The new build did not say its state key: resume plainly and say so.
    ResumeUnverified,
}

/// Chooses the reload path from facts alone, so it is testable without exec.
pub(crate) fn reload_plan(
    resumable: bool,
    running_key: &str,
    installed_key: Option<&str>,
) -> ReloadPlan {
    if !resumable {
        return ReloadPlan::Fresh;
    }
    match installed_key {
        Some(key) if key == running_key => ReloadPlan::Resume,
        Some(_) => ReloadPlan::Carry,
        None => ReloadPlan::ResumeUnverified,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryCapacityProbe {
    pub(crate) route: String,
    pub(crate) total_window: Option<u64>,
    pub(crate) admitted_input: Option<u64>,
    pub(crate) estimated_tokens: u64,
    pub(crate) messages: usize,
}
impl CarryCapacityProbe {
    pub(crate) fn warning(&self) -> Option<String> {
        (self.admitted_input.is_some_and(|input|self.estimated_tokens > input)).then(|| format!(
            "estimated history ~{} tokens exceeds destination route {} input window {} tokens (total window {}); the first prompt will refuse unless the history compacts",
            self.estimated_tokens,self.route,self.admitted_input.unwrap_or(0),
            self.total_window.map_or_else(|| "not reported".to_owned(), |n|format!("{n} tokens")),
        ))
    }
}

/// The installed `plak` a reload would start, and the state key it answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReloadTarget {
    pub(crate) executable: PathBuf,
    pub(crate) capacity: Option<Box<CarryCapacityProbe>>,
    pub(crate) capacity_error: Option<String>,
    /// `None` when the build did not answer in time or with one key.
    pub(crate) state_key: Option<String>,
    pub(crate) probe: Option<ReloadProbe>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(crate) struct ReloadProbe {
    pub(crate) state_key: String,
    pub(crate) export_versions: Vec<u32>,
    pub(crate) config_error: Option<String>,
}

pub(crate) fn reload_probe(paths: &PlakPaths) -> ReloadProbe {
    ReloadProbe {
        state_key: crate::home::state::state_key(),
        export_versions: IMPORTABLE_VERSIONS.to_vec(),
        config_error: crate::product_config::SessionPolicy::load(paths).err(),
    }
}

impl ReloadTarget {
    pub(crate) fn refusal(&self, running_key: &str) -> Option<String> {
        if let Some(error) = &self.capacity_error {
            return Some(error.clone());
        }
        let probe = self.probe.as_ref()?;
        if let Some(error) = &probe.config_error {
            return Some(format!(
                "reload kept this session open: {error}; fix the named file and retry /reload"
            ));
        }
        if probe.state_key != running_key && !probe.export_versions.contains(&VERSION) {
            return Some(format!(
                "reload kept this session open: the new build cannot import this build's version {VERSION} export; install a build that imports it and retry /reload"
            ));
        }
        None
    }
}

/// Resolves the installed `plak` and asks its state key; `None` when nothing
/// is installed on `PATH`.
pub(crate) fn reload_target(workspace: &Path) -> Option<ReloadTarget> {
    let executable = crate::build_identity::installed_executable()?;
    Some(probe_reload_target(executable, workspace))
}

pub(crate) fn probe_reload_target(executable: PathBuf, workspace: &Path) -> ReloadTarget {
    probe_reload_target_with_timeout(executable, workspace, STATE_KEY_TIMEOUT)
}

fn probe_reload_target_with_timeout(
    executable: PathBuf,
    workspace: &Path,
    timeout: Duration,
) -> ReloadTarget {
    let mut command = std::process::Command::new(&executable);
    command
        .arg("version")
        .arg("--reload-probe")
        .current_dir(workspace);
    let Some(output) = run_bounded(&mut command, timeout).ok().flatten() else {
        // A hung destination has spent its budget; do not ask it a second question.
        return ReloadTarget {
            executable,
            capacity: None,
            capacity_error: None,
            state_key: None,
            probe: None,
        };
    };
    let probe = Some(output)
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<ReloadProbe>(&output.stdout).ok())
        .filter(|probe| is_state_key(&probe.state_key));
    let state_key = probe
        .as_ref()
        .map(|probe| probe.state_key.clone())
        .or_else(|| installed_state_key(&executable));
    ReloadTarget {
        executable,
        capacity: None,
        capacity_error: None,
        state_key,
        probe,
    }
}

/// Resolves carry capacity while the old session is still attached.
pub(crate) fn reload_target_for_session(
    workspace: &Path,
    session_id: Option<&str>,
) -> Option<ReloadTarget> {
    let mut target = reload_target(workspace)?;
    if target.refusal(&crate::home::state::state_key()).is_some()
        || target.state_key.as_deref() == Some(crate::home::state::state_key().as_str())
        || target.state_key.is_none()
    {
        return Some(target);
    }
    let Some(id) = session_id else {
        return Some(target);
    };
    let inspect = || -> Result<CarryCapacityProbe, String> {
        let services = Services::from_env(None);
        let paths = services.paths().map_err(|e| e.message().to_owned())?;
        let record = services
            .session_records()
            .map_err(|e| e.message().to_owned())?
            .load(id)?;
        let document = export_session(paths, &record).map_err(|e| e.to_string())?;
        let path = new_carry_file(paths, id)?;
        write_document(&path, &document)?;
        let mut command = std::process::Command::new(&target.executable);
        command
            .arg("version")
            .arg("--carry-probe")
            .arg(&path)
            .current_dir(workspace);
        let output = run_bounded(&mut command, SNAPSHOT_EXPORT_TIMEOUT);
        let _ = fs::remove_file(&path);
        let output = output
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "destination capacity inspection timed out".to_owned())?;
        if !output.status.success() {
            let cause = String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("destination gave no diagnostic")
                .chars()
                .take(512)
                .collect::<String>();
            let cause = safe_terminal_block(&cause);
            return Err(if output.status.code() == Some(2) {
                format!("destination build too old for v8 carry: {cause}")
            } else {
                format!(
                    "destination route capacity inspection failed: {cause}; fix configuration/auth and retry /reload"
                )
            });
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|_| "destination did not report carry capacity".to_owned())
    };
    match inspect() {
        Ok(capacity) => target.capacity = Some(Box::new(capacity)),
        Err(error) => {
            target.capacity_error = Some(format!("reload kept this session open: {error}"));
        }
    }
    Some(target)
}

/// How long an installed build gets to answer `version --state-key`.
const STATE_KEY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a kept snapshot gets to export one session.
const SNAPSHOT_EXPORT_TIMEOUT: Duration = Duration::from_secs(120);

/// Asks `executable` for its state key with the hidden `version --state-key`.
/// A build that predates the question, answers late, or answers with anything
/// but one key reads as unknown.
pub(crate) fn installed_state_key(executable: &Path) -> Option<String> {
    let mut command = std::process::Command::new(executable);
    command.arg("version").arg("--state-key");
    let output = run_bounded(&mut command, STATE_KEY_TIMEOUT).ok()??;
    if !output.status.success() {
        return None;
    }
    let key = String::from_utf8(output.stdout).ok()?;
    let key = key.trim();
    is_state_key(key).then(|| key.to_owned())
}

/// Runs `command` with no stdin and captured output, killing it once
/// `timeout` passes. `Ok(None)` means it was killed.
fn run_bounded(
    command: &mut std::process::Command,
    timeout: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::io::Read as _;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(status.map(|status| std::process::Output {
        status,
        stdout,
        stderr,
    }))
}

fn is_state_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
}

/// The newest kept snapshot whose `state-key` manifest names `key`, under
/// any of `roots` (the maintainer and release snapshot roots). Only installed
/// snapshot directories count (a 64-digit hex name, not a staging copy), and
/// only when this user owns them and their executable and nobody else can
/// write either.
pub(crate) fn snapshot_for_key(roots: &[PathBuf], key: &str) -> Option<PathBuf> {
    roots
        .iter()
        .filter_map(|root| fs::read_dir(root).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.len() != 64
                || !name
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return None;
            }
            let dir = entry.path();
            let executable = dir.join("plak");
            let dir_metadata = fs::symlink_metadata(&dir).ok()?;
            let executable_metadata = fs::symlink_metadata(&executable).ok()?;
            if !dir_metadata.is_dir()
                || !executable_metadata.is_file()
                || !privately_owned(&dir_metadata)
                || !privately_owned(&executable_metadata)
            {
                return None;
            }
            let manifest = fs::read_to_string(dir.join("state-key")).ok()?;
            (manifest.trim() == key).then(|| Some((dir_metadata.modified().ok()?, executable)))?
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, executable)| executable)
}

/// Owned by this user and writable by nobody else.
fn privately_owned(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

/// Runs `snapshot session export <id> --out <out>` in `workspace`, so the
/// build that owns the session writes its export. A snapshot that does not
/// finish in time is killed and the export fails.
pub(crate) fn export_with_snapshot(
    snapshot: &Path,
    session_id: &str,
    out: &Path,
    workspace: &Path,
) -> Result<(), String> {
    let result = export_with_snapshot_inner(snapshot, session_id, out, workspace);
    if result.is_err() {
        let _ = fs::remove_file(out);
    }
    result
}

fn export_with_snapshot_inner(
    snapshot: &Path,
    session_id: &str,
    out: &Path,
    workspace: &Path,
) -> Result<(), String> {
    let mut command = std::process::Command::new(snapshot);
    command
        .arg("session")
        .arg("export")
        .arg(session_id)
        .arg("--out")
        .arg(out)
        .current_dir(workspace);
    let output = run_bounded(&mut command, SNAPSHOT_EXPORT_TIMEOUT)
        .map_err(|error| format!("could not run {}: {error}", snapshot.display()))?
        .ok_or_else(|| {
            format!(
                "{} did not export session {session_id} within {} seconds",
                snapshot.display(),
                SNAPSHOT_EXPORT_TIMEOUT.as_secs()
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{} could not export session {session_id}: {}",
        snapshot.display(),
        stderr.trim()
    ))
}

pub(crate) fn snapshot_export_version(snapshot: &Path) -> Result<Option<u32>, String> {
    let Some(parent) = snapshot.parent() else {
        return Ok(None);
    };
    let path = parent.join("export-version");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    Ok(text.trim().parse::<u32>().ok())
}

/// The session in `workspace` already carried forward from `source_id` of
/// state `source_key`, if any. Carrying the same session twice would fork its
/// conversation into two copies, so callers reuse this one instead.
pub(crate) fn already_carried(
    store: &dyn crate::store::session_records::SessionRecordStore,
    workspace: &Path,
    source_id: &str,
    source_key: &str,
) -> Result<Option<SessionRecord>, String> {
    Ok(store
        .list_for_workspace(workspace)?
        .into_iter()
        .find(|record| record.carried_forward_from() == Some((source_id, source_key))))
}

/// The link a carried-forward record shows in listings and the TUI.
pub(crate) fn carried_forward_label(record: &SessionRecord) -> Option<String> {
    record
        .carried_forward_from()
        .map(|(id, key)| format!("carried forward from {id} ({key})"))
}

/// Exports `record` into a fresh private carry file and returns its path.
pub(crate) fn carry_out(
    paths: &PlakPaths,
    record: &SessionRecord,
) -> Result<(PathBuf, String), String> {
    let document = export_session(paths, record)
        .map_err(|refusal| format!("session {} cannot be exported: {refusal}", record.id))?;
    let path = new_carry_file(paths, &record.id)?;
    write_document(&path, &document)?;
    let digest = plakous::model::sha256_hex(&[&fs::read(&path).map_err(|e| e.to_string())?]);
    if let Err(error) = write_private_new(&path.with_extension("handoff"), digest.as_bytes()) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok((path, document.carry_summary()))
}

fn consume_handoff_receipt(path: &Path, home: &Path, digest: Option<&str>) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(parent) = fs::canonicalize(home.join("carry")) else {
        return false;
    };
    if path
        .parent()
        .and_then(|p| fs::canonicalize(p).ok())
        .as_ref()
        != Some(&parent)
    {
        return false;
    }
    let Ok(directory) = fs::metadata(&parent) else {
        return false;
    };
    if !privately_owned(&directory) || directory.mode() & 0o077 != 0 {
        return false;
    }
    let receipt = path.with_extension("handoff");
    let Ok(metadata) = fs::symlink_metadata(&receipt) else {
        return false;
    };
    if !metadata.is_file()
        || !privately_owned(&metadata)
        || metadata.mode() & 0o077 != 0
        || metadata.len() != 64
    {
        return false;
    }
    let expected = fs::read_to_string(&receipt);
    let _ = fs::remove_file(&receipt);
    matches!((expected,digest),(Ok(expected),Some(digest)) if expected==digest)
}

/// `plak --resume` of a session another build's state holds. When a kept
/// maintainer snapshot of that build exists, it exports the session and this
/// build imports it as a new session; the new id and its note are returned.
/// Without one the caller's refusal stands.
pub(crate) fn carry_foreign_session(
    services: &Services,
    paths: &PlakPaths,
    cwd: &Path,
    requested: &str,
) -> CliResult<Option<(String, String)>> {
    let Some((key, id)) = crate::home::state::foreign_session(paths, requested) else {
        return Ok(None);
    };
    if let Some(existing) = already_carried(services.session_records()?.as_ref(), cwd, &id, &key)
        .map_err(CliFailure::new)?
    {
        let note = format!(
            "session {id} ({key}) was already carried forward as {}; resuming it",
            existing.id
        );
        return Ok(Some((existing.id, note)));
    }
    let Some(snapshot) = snapshot_for_key(&crate::home::paths::installed_snapshot_roots(), &key)
    else {
        return Ok(None);
    };
    let workspace_paths = PlakPaths::for_workspace(paths.home().to_path_buf(), cwd);
    crate::product_config::SessionPolicy::load(&workspace_paths).map_err(|error| {
        CliFailure::new(format!(
            "could not carry session {id} forward: {error}; fix the config and retry the same command"
        ))
    })?;
    let export_version = snapshot_export_version(&snapshot).map_err(|error| {
        CliFailure::new(format!(
            "could not carry session {id} forward: {error}; fix the error and retry the same command"
        ))
    })?;
    if let Some(version) = export_version
        && !IMPORTABLE_VERSIONS.contains(&version)
    {
        return Err(CliFailure::new(format!(
            "could not carry session {id} forward: the kept snapshot writes version {version} session exports; this build imports versions {:?}; use this workspace's newest carried session from `plak session list`",
            IMPORTABLE_VERSIONS
        )));
    }
    let out = new_carry_file(paths, &id).map_err(CliFailure::new)?;
    let result = export_with_snapshot(&snapshot, &id, &out, cwd)
        .map_err(CliFailure::new)
        .and_then(|()| {
            let document = read_document(&out).map_err(|error| {
                CliFailure::new(format!(
                    "{error}; use this workspace's newest carried session from `plak session list`"
                ))
            })?;
            commands::session::import_document(services, document, false).map_err(|failure| {
                CliFailure::new(format!(
                    "{}; fix the error and retry the same command",
                    failure.message()
                ))
            })
        });
    let (record, note) = result.map_err(|failure| {
        let cleanup = match fs::remove_file(&out) {
            Ok(()) => String::new(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => format!("; could not delete {}: {error}", out.display()),
        };
        CliFailure::new(format!(
            "could not carry session {id} forward: {}{cleanup}",
            failure.message()
        ))
    })?;
    let note = match fs::remove_file(&out) {
        Ok(()) => note,
        Err(error) => format!("{note}; could not delete {}: {error}", out.display()),
    };
    Ok(Some((record.id, note)))
}

/// `plak --import-resume <file>`: the new build's half of a carrying
/// `/reload`. The file is deleted once the import committed and kept, with
/// its path named, when it did not.
pub(crate) fn import_and_resume(services: &Services, path: &Path) -> CliResult<ProcessOutcome> {
    let document = read_document(path).map_err(|failure| {
        CliFailure::new(format!(
            "could not carry the session forward: {failure}; the export is kept at {}",
            path.display()
        ))
    })?;
    let source_id = document.source().0.to_owned();
    let local_handoff = consume_handoff_receipt(
        path,
        services.paths()?.home(),
        document.local_digest.as_deref(),
    );
    let paths = PlakPaths::for_workspace(
        services.paths()?.home().to_path_buf(),
        document.workspace_root(),
    );
    crate::product_config::SessionPolicy::load(&paths).map_err(CliFailure::new)?;
    if !rustix::termios::isatty(std::io::stdin()) || !std::io::stdout().is_terminal() {
        return Err(CliFailure::classified(
            CliFailureKind::Usage,
            "the interactive TUI needs a terminal; import was refused before creating a successor",
        ));
    }
    let cwd = env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    let requested = fs::canonicalize(&cwd).unwrap_or(cwd);
    let recorded = fs::canonicalize(document.workspace_root())
        .unwrap_or_else(|_| document.workspace_root().to_path_buf());
    if requested != recorded {
        return Err(CliFailure::new(format!(
            "session {source_id} belongs to {}; import-resume from {} was refused before creating a successor",
            recorded.display(),
            requested.display()
        )));
    }
    crate::home::state::ensure_usable_state(services.paths()?, &mut std::io::stderr().lock())
        .map_err(CliFailure::new)?;
    let (record, note) = commands::session::import_document(services, document, local_handoff)
        .map_err(|failure| {
            CliFailure::new(format!(
                "could not carry the session forward: {}; the export is kept at {}",
                failure.message(),
                path.display()
            ))
        })?;
    let note = match fs::remove_file(path) {
        Ok(()) => note,
        Err(error) => format!("{note}; could not delete {}: {error}", path.display()),
    };
    let successor = record.id;
    crate::tui_launch::launch_tui(services, Some(successor.clone()), false, Some(note))
        .map_err(|failure| import_resume_failure(failure, &successor, &source_id))
}

fn import_resume_failure(failure: CliFailure, successor: &str, source: &str) -> CliFailure {
    CliFailure::new(format!(
        "{}; session {successor} was carried forward from {source}; fix the error and run: plak --resume {successor}",
        failure.message()
    ))
}

fn raw_history_field<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<&'de serde_json::value::RawValue>, D::Error> {
    <&serde_json::value::RawValue>::deserialize(d).map(Some)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use plakous_hosted::sessions::{ThinkingRow, ToolItem};

    #[test]
    fn readable_spellings_pin_every_variant_to_the_previous_debug_output() {
        for actor in [
            Actor::Developer,
            Actor::System,
            Actor::User,
            Actor::Assistant,
            Actor::Tool,
            Actor::Harness,
        ] {
            assert_eq!(actor.as_str(), format!("{actor:?}").to_ascii_lowercase());
        }
        for status in [
            NodeStatus::Pending,
            NodeStatus::Running,
            NodeStatus::Paused,
            NodeStatus::Sealing,
            NodeStatus::Completed,
            NodeStatus::Interrupted,
            NodeStatus::Failed,
            NodeStatus::Closed,
            NodeStatus::RecoveryRequired,
        ] {
            assert_eq!(node_status_name(status), format!("{status:?}"));
        }
        for status in [
            WorkspaceStatus::Active,
            WorkspaceStatus::Adopting,
            WorkspaceStatus::Discarding,
            WorkspaceStatus::CleanRemoved,
            WorkspaceStatus::Adopted,
            WorkspaceStatus::Discarded,
        ] {
            assert_eq!(workspace_status_name(status), format!("{status:?}"));
        }
        for mode in [
            WorkspaceMode::Inherit,
            WorkspaceMode::Snapshot,
            WorkspaceMode::Fork,
        ] {
            assert_eq!(workspace_mode_name(mode), format!("{mode:?}"));
        }
    }

    #[test]
    fn reload_resumes_plainly_on_the_same_key_and_carries_across_keys() {
        assert_eq!(reload_plan(false, "a", Some("b")), ReloadPlan::Fresh);
        assert_eq!(reload_plan(true, "a", Some("a")), ReloadPlan::Resume);
        assert_eq!(reload_plan(true, "a", Some("b")), ReloadPlan::Carry);
        assert_eq!(reload_plan(true, "a", None), ReloadPlan::ResumeUnverified);
    }

    #[test]
    fn reload_probe_checks_config_and_carry_versions_before_handoff() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("plak");
        let key = crate::home::state::state_key();
        let probe = ReloadProbe {
            state_key: key.clone(),
            export_versions: IMPORTABLE_VERSIONS.to_vec(),
            config_error: Some("config.toml: unknown key; run `plak config edit`".into()),
        };
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s' '{}'\n",
                serde_json::to_string(&probe).unwrap()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut target = probe_reload_target(executable, root.path());
        assert_eq!(target.state_key.as_deref(), Some(key.as_str()));
        for running in [&key, "different-key"] {
            let refusal = target.refusal(running).unwrap();
            assert!(refusal.contains("plak config edit"));
            assert!(refusal.contains("retry /reload"));
        }
        target.probe.as_mut().unwrap().config_error = None;
        assert!(target.refusal(&key).is_none());
        target.probe.as_mut().unwrap().export_versions = vec![VERSION + 1];
        assert!(target.refusal(&key).is_none());
        assert!(
            target
                .refusal("different-key")
                .unwrap()
                .contains("cannot import")
        );
    }

    #[test]
    fn reload_probe_falls_back_to_the_old_state_key_answer() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("plak");
        let key = crate::home::state::state_key();
        fs::write(
            &executable,
            format!("#!/bin/sh\n[ \"$2\" = --state-key ] || exit 2\necho {key}\n"),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let target = probe_reload_target(executable, root.path());
        assert_eq!(target.state_key.as_deref(), Some(key.as_str()));
        assert!(target.probe.is_none());
        assert!(target.refusal(&key).is_none());
    }

    #[test]
    fn snapshot_export_version_manifest_treats_missing_and_unparseable_as_unknown() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = root.path().join("plak");
        assert_eq!(snapshot_export_version(&snapshot).unwrap(), None);
        for version in [PREVIOUS_VERSION, VERSION, 999] {
            fs::write(root.path().join("export-version"), format!("{version}\n")).unwrap();
            assert_eq!(snapshot_export_version(&snapshot).unwrap(), Some(version));
        }
        for manifest in ["", "not a version", "4\n5", "4294967296"] {
            fs::write(root.path().join("export-version"), manifest).unwrap();
            assert_eq!(snapshot_export_version(&snapshot).unwrap(), None);
        }
    }

    #[test]
    fn timed_out_reload_probe_does_not_launch_a_fallback_process() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("plak");
        let calls = root.path().join("calls");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\necho called >> '{}'\nexec sleep 30\n",
                calls.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let target =
            probe_reload_target_with_timeout(executable, root.path(), Duration::from_secs(2));
        assert!(target.probe.is_none());
        assert!(target.state_key.is_none());
        assert_eq!(fs::read_to_string(calls).unwrap(), "called\n");
    }

    #[test]
    fn published_export_survives_directory_sync_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("export.json");
        let document = sample_document();
        write_document_with_directory_sync(&path, &document, |_| {
            Err(std::io::Error::other("synthetic directory sync failure"))
        })
        .unwrap();
        let published = read_document(&path).unwrap();
        assert_eq!(published.source(), document.source());
        assert!(write_document(&path, &document).is_err());
    }

    #[test]
    fn only_a_bare_state_key_is_an_answer() {
        assert!(is_state_key("r20.e16.p25.t2"));
        assert!(!is_state_key(""));
        assert!(!is_state_key("plak 0.1.0 (release; binary abc)"));
        assert!(!is_state_key("../state"));
    }

    #[test]
    fn the_newest_private_installed_snapshot_for_a_key_is_chosen() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let snapshot = |name: &str, key: &str| {
            let dir = root.path().join(name);
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(dir.join("state-key"), format!("{key}\n")).unwrap();
            fs::write(dir.join("plak"), b"#!/bin/sh\n").unwrap();
            fs::set_permissions(dir.join("plak"), fs::Permissions::from_mode(0o555)).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            dir
        };
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        snapshot(&a, "r1.e1.p1.t1");
        snapshot(&b, "r2.e2.p2.t2");
        snapshot(&c, "r1.e1.p1.t1");
        // Newer, but not an installed snapshot or not private: never chosen.
        snapshot(".staging.abcdef", "r1.e1.p1.t1");
        snapshot("not-a-snapshot", "r1.e1.p1.t1");
        let open = snapshot(&"d".repeat(64), "r1.e1.p1.t1");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let writable = snapshot(&"e".repeat(64), "r1.e1.p1.t1");
        fs::set_permissions(writable.join("plak"), fs::Permissions::from_mode(0o757)).unwrap();
        fs::create_dir(root.path().join("f".repeat(64))).unwrap();
        assert_eq!(
            snapshot_for_key(&[root.path().to_path_buf()], "r1.e1.p1.t1"),
            Some(root.path().join(&c).join("plak"))
        );
        assert_eq!(
            snapshot_for_key(&[root.path().to_path_buf()], "r2.e2.p2.t2"),
            Some(root.path().join(&b).join("plak"))
        );
        assert_eq!(
            snapshot_for_key(&[root.path().to_path_buf()], "r9.e9.p9.t9"),
            None
        );
    }

    #[test]
    fn snapshots_under_every_installed_root_are_searched() {
        use std::os::unix::fs::PermissionsExt as _;
        let maintainer = tempfile::tempdir().unwrap();
        let release = tempfile::tempdir().unwrap();
        let snapshot = |root: &Path, name: &str, key: &str| {
            let dir = root.join(name);
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(dir.join("state-key"), format!("{key}\n")).unwrap();
            fs::write(dir.join("plak"), b"#!/bin/sh\n").unwrap();
            fs::set_permissions(dir.join("plak"), fs::Permissions::from_mode(0o555)).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            dir.join("plak")
        };
        let roots = [
            maintainer.path().to_path_buf(),
            release.path().to_path_buf(),
        ];
        let only_release = snapshot(release.path(), &"a".repeat(64), "r1.e1.p1.t1");
        assert_eq!(snapshot_for_key(&roots, "r1.e1.p1.t1"), Some(only_release));
        snapshot(release.path(), &"b".repeat(64), "r2.e2.p2.t2");
        let newer_maintainer = snapshot(maintainer.path(), &"c".repeat(64), "r2.e2.p2.t2");
        assert_eq!(
            snapshot_for_key(&roots, "r2.e2.p2.t2"),
            Some(newer_maintainer)
        );
        let missing = [
            maintainer.path().join("missing"),
            release.path().to_path_buf(),
        ];
        assert!(snapshot_for_key(&missing, "r1.e1.p1.t1").is_some());
    }

    #[test]
    fn a_snapshot_that_hangs_is_killed_and_reads_as_unknown() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let slow = root.path().join("plak");
        fs::write(&slow, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let mut command = std::process::Command::new(&slow);
        assert!(
            run_bounded(&mut command, Duration::from_millis(200))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn inert_children_are_archivable_but_live_or_sealing_children_refuse() {
        assert_eq!(
            count_archivable_children(
                [
                    ("a", NodeStatus::Completed),
                    ("b", NodeStatus::Failed),
                    ("c", NodeStatus::Closed),
                    ("d", NodeStatus::RecoveryRequired),
                    ("e", NodeStatus::Interrupted),
                ]
                .into_iter()
            ),
            Ok(5)
        );
        for status in [
            NodeStatus::Pending,
            NodeStatus::Running,
            NodeStatus::Paused,
            NodeStatus::Sealing,
        ] {
            assert_eq!(
                count_archivable_children([("live", status)].into_iter()),
                Err(ExportRefusal::SubagentRunning {
                    agent_id: "live".to_owned(),
                    status
                })
            );
        }
    }

    #[test]
    fn tree_reference_facts_omit_settled_rows_and_count_them() {
        use plakous_hosted::agent_tree_protocol::{AttemptScopedRequest, SpawnAgentRequest};
        let (_home, paths) = PlakPaths::isolated_test_home();
        let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
        paths.ensure_layout().unwrap();
        let record = SessionRecord::new(repo.path().to_path_buf());
        let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
            &record.id,
            repo.path().to_path_buf(),
        );
        let completed = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        let closed = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.record_turn_completed(&completed.agent_id).unwrap();
        tree.close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: closed.agent_id.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        let (count, resolved, facts) = tree_reference_facts(&paths, &record).unwrap();
        assert_eq!(count, 2);
        assert!(facts.is_empty());
        assert_eq!(resolved, 3);
        fs::write(repo.path().join("uncommitted.txt"), "retained changes").unwrap();
        let (_, _, facts) = tree_reference_facts(&paths, &record).unwrap();
        assert!(facts.iter().any(|entry| entry.kind == "source_workspace"));
        assert!(!facts.iter().any(|entry| entry.kind == "source_child"));
    }

    #[test]
    fn closed_child_dirty_copy_remains_in_reference_ledger() {
        use plakous_hosted::agent_tree_protocol::{AttemptScopedRequest, SpawnAgentRequest, Tier};
        let (_home, paths) = PlakPaths::isolated_test_home();
        let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
        paths.ensure_layout().unwrap();
        let record = SessionRecord::new(repo.path().to_path_buf());
        let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
            &record.id,
            repo.path().to_path_buf(),
        );
        let child = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "editor", "edit")
                    .with_fork(),
                None,
            )
            .unwrap();
        fs::write(
            tree.node(&child.agent_id)
                .unwrap()
                .path
                .join("retained.txt"),
            "retained work",
        )
        .unwrap();
        tree.close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: child.agent_id.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
        tree.settle_for_test();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        let (_, _, facts) = tree_reference_facts(&paths, &record).unwrap();
        assert!(
            facts
                .iter()
                .any(|entry| entry.kind == "source_workspace"
                    && entry.operation == child.workspace.id)
        );
        assert!(facts.iter().any(|entry| entry.kind == "source_candidate"));
        assert!(!facts.iter().any(|entry| entry.kind == "source_child"));
    }

    #[test]
    fn a_command_or_workflow_intent_without_a_fact_refuses_export() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let mut record = SessionRecord::new(root.path().to_path_buf());
        record.plakous_locator = Some(test_locator());
        let projected =
            |commands: &[&str], workflows: &[&str]| plakous_hosted::runtime::ProjectedSession {
                head: test_locator(),
                pending_commands: commands.iter().map(|id| (*id).to_owned()).collect(),
                pending_workflows: workflows.iter().map(|id| (*id).to_owned()).collect(),
            };
        let refusal =
            export_projected(&paths, &record, &projected(&["plak:command"], &[])).unwrap_err();
        assert_eq!(refusal, ExportRefusal::UnsettledIntent);
        assert!(
            refusal
                .to_string()
                .contains("open the session once to settle it")
        );
        assert_eq!(
            export_projected(&paths, &record, &projected(&[], &["plak:compact"])).unwrap_err(),
            ExportRefusal::UnsettledIntent
        );
    }

    #[test]
    fn a_fake_snapshot_answers_its_key_and_exports_through_its_own_cli() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let snapshot = root.path().join("plak");
        fs::write(
            &snapshot,
            "#!/bin/sh\n\
             case \"$1 $2\" in\n\
             'version --state-key') echo r1.e1.p1.t1 ;;\n\
             'session export') [ \"$3\" = good ] || { echo \"no session $3\" >&2; exit 2; }\n\
               pwd > \"$5\" ;;\n\
             *) exit 9 ;;\n\
             esac\n",
        )
        .unwrap();
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            installed_state_key(&snapshot).as_deref(),
            Some("r1.e1.p1.t1")
        );
        assert_eq!(installed_state_key(&root.path().join("missing")), None);

        let out = root.path().join("out.json");
        export_with_snapshot(&snapshot, "good", &out, root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            root.path().canonicalize().unwrap().to_str().unwrap()
        );
        let failure = export_with_snapshot(&snapshot, "bad", &out, root.path()).unwrap_err();
        assert!(
            failure.ends_with("could not export session bad: no session bad"),
            "{failure}"
        );
    }

    #[test]
    fn a_failed_snapshot_export_does_not_leave_a_partial_carry_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let snapshot = root.path().join("plak");
        fs::write(&snapshot, "#!/bin/sh\necho partial > \"$5\"\nexit 2\n").unwrap();
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
        let out = root.path().join("carry.json");
        assert!(export_with_snapshot(&snapshot, "source", &out, root.path()).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn carry_files_are_private_and_never_overwritten() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        fs::create_dir(paths.home()).unwrap();
        let path = new_carry_file(&paths, "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11").unwrap();
        let dir = path.parent().unwrap();
        assert_eq!(dir, paths.home().join("carry"));
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let document = sample_document();
        write_document(&path, &document).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_document(&path, &document).is_err());
        let read = read_document(&path).unwrap();
        assert_eq!(read.source().0, document.source.session_id);
    }

    /// Every stored row type survives an export and its import unchanged.
    #[test]
    fn previous_document_preserves_every_transcript_row_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.json");
        let mut document = sample_document();
        document.transcript = crate::store::every_stored_row_type();
        fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let imported = read_document(&path).unwrap();
        assert_eq!(imported.version, PREVIOUS_VERSION);
        assert_eq!(imported.transcript, crate::store::every_stored_row_type());
    }

    #[test]
    fn previous_document_carries_typed_rows_and_refuses_v5() {
        assert_eq!(VERSION, 8);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.json");
        let mut value = serde_json::to_value(sample_document()).unwrap();
        assert_eq!(
            value["transcript"],
            serde_json::json!([{"kind": "user", "text": "first"}])
        );
        value["version"] = serde_json::json!(5);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            read_document(&path)
                .unwrap_err()
                .to_string()
                .contains("version 5")
        );
    }

    #[test]
    fn previous_export_fixture_imports_as_the_rows_it_carried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.json");
        fs::write(
            &path,
            include_str!("../tests/fixtures/session-export-v7.json"),
        )
        .unwrap();
        let document = read_document(&path).unwrap();
        assert_eq!(document.version, PREVIOUS_VERSION);
        assert_eq!(document.source_resolved_count, Some(3));
        assert_eq!(document.transcript[0], TranscriptRow::user("first prompt"));
        // An absent exposed name renders as the canonical tool name.
        assert!(matches!(
            &document.transcript[1],
            TranscriptRow::Tool(tool) if tool.name == "loopback_effect"
                && tool.exposed_name.is_none()
                && tool.label() == "loopback_effect"
                && tool.state == "succeeded"
        ));
        assert!(
            document
                .transcript
                .contains(&TranscriptRow::assistant("first answer"))
        );
        let exported = serde_json::to_value(document).unwrap();
        assert_eq!(exported["version"], PREVIOUS_VERSION);
        assert_eq!(
            exported["transcript"][0],
            serde_json::json!({"kind": "user", "text": "first prompt"})
        );
    }

    /// A version 7 tool row whose exposed name equals its canonical name
    /// imports with the exposed name absent.
    #[test]
    fn a_version_7_redundant_exposed_name_imports_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.json");
        let row = serde_json::json!({
            "kind": "tool", "call_id": "c1", "name": "apply_patch",
            "exposed_name": "apply_patch", "activity_kind": "tool", "state": "succeeded"
        });
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/session-export-v7.json")).unwrap();
        value["version"] = serde_json::json!(PREVIOUS_VERSION);
        value["transcript"] = serde_json::json!([row]);
        fs::write(&path, value.to_string()).unwrap();
        let document = read_document(&path).unwrap();
        let [TranscriptRow::Tool(tool)] = document.transcript.as_slice() else {
            panic!("one tool row: {:?}", document.transcript);
        };
        assert_eq!(tool.exposed_name, None);
        assert_eq!(tool.label(), "apply_patch");
        let CarriedImport { record, .. } = document.into_import().unwrap();
        assert!(matches!(
            record.transcript.to_vec().as_slice(),
            [TranscriptRow::Tool(tool)] if tool.exposed_name.is_none()
        ));
    }

    #[test]
    fn previous_document_carries_send_times_and_process_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.json");
        let mut document = sample_document();
        document.transcript = vec![
            TranscriptRow::User {
                text: "first".into(),
                sent_at_ms: Some(1_791_383_520_000),
            },
            TranscriptRow::Process(plakous_host_protocol::views::transcript::ProcessNotice {
                job_id: "p261".into(),
                outcome: plakous_host_protocol::views::transcript::ProcessOutcome::Exited {
                    code: Some(0),
                },
                duration_ms: Some(1_300_000),
                subject: Some("just check".into()),
            }),
        ];
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(value["transcript"][0]["sent_at_ms"], 1_791_383_520_000_u64);
        assert_eq!(value["transcript"][1]["subject"], "just check");
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            read_document(&path).unwrap().transcript,
            document.transcript
        );
    }

    #[test]
    fn previous_document_rejects_resolved_count_pseudo_work() {
        let mut value = serde_json::to_value(sample_document()).unwrap();
        value["pending_work"] = serde_json::json!([{
            "operation": "resolved_source_work", "kind": "source_resolved_count",
            "state": "3", "detail": "settled child/candidate/workspace rows omitted"
        }]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_document(&path).is_err());
    }

    #[test]
    fn another_version_or_format_is_refused_by_name() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("doc.json");
        let mut value = serde_json::to_value(sample_document()).unwrap();
        value["version"] = serde_json::json!(9);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = read_document(&path).unwrap_err();
        assert!(error.to_string().contains("version 9"), "{error}");
        value["format"] = serde_json::json!("something-else");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            read_document(&path)
                .unwrap_err()
                .to_string()
                .contains("not a session export")
        );
    }

    /// The session header is derived from the record and never stored (spec
    /// 02 §5.4): a document that plants one naming another session and
    /// source must not render it under the real header.
    #[test]
    fn an_import_never_stores_a_session_header_row() {
        let mut document = sample_document();
        document.transcript.push(TranscriptRow::SessionHeader {
            session_id: "00000000-0000-4000-8000-000000000000".into(),
            carried_from: None,
        });
        let CarriedImport { record, .. } = document.into_import().unwrap();
        assert!(
            !record
                .transcript
                .iter()
                .any(|row| matches!(row, TranscriptRow::SessionHeader { .. })),
            "a stored header row would render as a second session header"
        );
    }

    /// Imported rows take the path live rows take: a truncation marker stays
    /// only as the first row, and tool and thinking rows are bounded exactly
    /// as the host bounds them before it persists them.
    #[test]
    fn an_import_bounds_rows_the_way_live_rows_are_bounded() {
        let mut document = sample_document();
        document.transcript = vec![
            TranscriptRow::Truncated,
            TranscriptRow::user("kept"),
            TranscriptRow::Truncated,
            TranscriptRow::Tool(ToolItem {
                call_id: "call-big".into(),
                name: "exec_command".into(),
                exposed_name: None,
                kind: plakous_host_protocol::views::presentation::ActivityKind::Process,
                state: "succeeded".into(),
                subject: Some(format!("first line\n{}", "s".repeat(1_000))),
                detail: Some("d".repeat(10 * 1024)),
                started_at_ms: None,
                finished_at_ms: None,
                parent_call_id: None,
                cell_id: None,
                wait: None,
            }),
            TranscriptRow::Thinking(ThinkingRow {
                location_key: "0:0".into(),
                text: "t".repeat(100 * 1024),
                elapsed_ms: None,
            }),
            TranscriptRow::Thinking(ThinkingRow {
                location_key: "0:1".into(),
                text: "  ".into(),
                elapsed_ms: None,
            }),
            TranscriptRow::Process(plakous_host_protocol::views::transcript::ProcessNotice {
                job_id: "p1".into(),
                outcome: plakous_host_protocol::views::transcript::ProcessOutcome::Stopped,
                duration_ms: None,
                subject: Some(format!("first line\n{}", "s".repeat(1_000))),
            }),
        ];
        let CarriedImport { record, .. } = document.into_import().unwrap();
        let rows = record.transcript.to_vec();
        assert_eq!(rows.len(), 5, "{rows:?}");
        let TranscriptRow::Process(process) = &rows[4] else {
            panic!("a process row: {:?}", rows[4]);
        };
        let subject = process.subject.as_deref().unwrap();
        assert!(subject.len() <= 240 && !subject.contains('\n'), "{subject}");
        assert_eq!(rows[0], TranscriptRow::Truncated);
        assert_eq!(rows[1], TranscriptRow::user("kept"));
        let TranscriptRow::Tool(tool) = &rows[2] else {
            panic!("a tool row: {:?}", rows[2]);
        };
        let subject = tool.subject.as_deref().unwrap();
        assert!(subject.len() <= 240 && !subject.contains('\n'), "{subject}");
        assert!(
            tool.detail.as_deref().unwrap().len()
                <= plakous_hosted::sessions::MAX_TOOL_ACTIVITY_DETAIL_BYTES
        );
        let TranscriptRow::Thinking(thinking) = &rows[3] else {
            panic!("a thinking row: {:?}", rows[3]);
        };
        assert!(thinking.text.len() <= plakous_hosted::sessions::MAX_THINKING_ACTIVITY_BYTES);
    }

    /// A document whose transcript holds more rows than any session can is
    /// refused while it is read.
    #[test]
    fn a_document_past_the_transcript_bound_is_refused() {
        let mut value = serde_json::to_value(sample_document()).unwrap();
        value["transcript"] = serde_json::Value::Array(
            (0..=MAX_TRANSCRIPT_LINES)
                .map(|_| serde_json::to_value(TranscriptRow::user("x")).unwrap())
                .collect(),
        );
        let error = serde_json::from_str::<CarryDocument>(&value.to_string()).unwrap_err();
        assert!(error.to_string().contains("more than"), "{error}");
        value["transcript"] = serde_json::Value::Array(
            (0..20)
                .map(|_| {
                    serde_json::to_value(TranscriptRow::user("x".repeat(1024 * 1024))).unwrap()
                })
                .collect(),
        );
        let error = serde_json::from_str::<CarryDocument>(&value.to_string()).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[test]
    fn an_import_links_back_and_holds_its_queue() {
        let CarriedImport {
            record,
            history,
            note,
            ..
        } = sample_document().into_import().unwrap();
        assert_eq!(
            note,
            format!(
                "session {} linked from 5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11 (r1.e1.p1.t1, producer binary abcdef012345); resumed from a bounded checkpoint, with the readable archive retained only as untrusted reference data",
                record.id
            )
        );
        assert_ne!(record.id, "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11");
        assert_eq!(
            record.carried_forward_from(),
            Some(("5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11", "r1.e1.p1.t1"))
        );
        assert_eq!(
            carried_forward_label(&record).as_deref(),
            Some("carried forward from 5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11 (r1.e1.p1.t1)")
        );
        record.validate_for_store().unwrap();
        assert_eq!(record.title.as_deref(), Some("carried"));
        assert_eq!(record.model.as_deref(), Some("offline-loopback-v1"));
        assert_eq!(
            record
                .pending_queue
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            ["held follow-up"]
        );
        assert_eq!(record.queue_drain_state, QueueDrainState::RestoredPaused);
        assert!(record.plakous_locator.is_none());
        assert_eq!(history.replay_account(), "");
    }

    #[test]
    fn export_refuses_a_session_without_a_runtime() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let record = SessionRecord::new(root.path().to_path_buf());
        assert_eq!(
            export_session(&paths, &record).unwrap_err(),
            ExportRefusal::NoRuntime
        );
    }

    #[test]
    fn golden_v7_document_imports_with_separate_reference_provenance() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("v7.json");
        fs::write(
            &path,
            include_str!("../tests/fixtures/session-export-v7.json"),
        )
        .unwrap();
        let document = read_document(&path).unwrap();
        assert_eq!(document.version, PREVIOUS_VERSION);
        assert_eq!(document.source_resolved_count, Some(3));
        let imported = document.into_import().unwrap();
        let segments = imported.history.segments();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].items()[0].actor(), Actor::User);
        assert_eq!(segments[1].items()[0].provenance(), Provenance::Imported);
        assert_eq!(
            readable_item_text(&segments[0].items()[0]),
            "Original user goal:\nfirst prompt\n\nRecent user goals or requests (newest first):\n- first prompt"
        );
        assert!(readable_item_text(&segments[1].items()[0]).contains("first answer"));
    }

    #[test]
    fn v7_import_intents_survive_reopen_and_rebuild() {
        assert_import_intents_rebuild(7);
    }

    fn assert_import_intents_rebuild(version: u32) {
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
            OpenSessionRequest, SessionBackend, SessionOperationId,
        };
        use plakous_hosted::session_store::SessionRecordStore;
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, text_runtime_binding};
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        paths.ensure_layout().unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/session-export-v7.json")).unwrap();
        value["version"] = serde_json::json!(version);
        value["workspace_root"] = serde_json::json!(root.path());
        value["queue"] = serde_json::json!(["carried queue stays paused"]);
        // A carried truncation marker is an intent too, not a disposable view.
        value["transcript"]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!({"kind": "truncated"}));
        let path = root.path().join("document.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let CarriedImport {
            mut record,
            history,
            ..
        } = read_document(&path).unwrap().into_import().unwrap();
        let expected = record.transcript.to_vec();
        let make_binding = || {
            text_runtime_binding(
                root.path(),
                BoundaryController::quiet_from_evidence_path(
                    CrashBoundary::None,
                    root.path().join("evidence"),
                )
                .unwrap(),
                "synthetic",
                "synthetic",
            )
            .unwrap()
        };
        let sessions =
            AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let CreateSessionOutcome::Created { session, .. } = executor
            .block_on(
                sessions.create(
                    CreateSessionRequest::new(
                        SessionOperationId::new(&record.create_operation_id).unwrap(),
                        make_binding(),
                    )
                    .with_history(history),
                ),
            )
            .unwrap()
            .unwrap()
        else {
            panic!("new import required");
        };
        record.plakous_locator = Some(executor.block_on(session.release()).unwrap().unwrap());
        let store: Arc<dyn SessionRecordStore> = Arc::new(
            crate::store::session_records::SqliteSessionStore::new(paths.clone()),
        );
        store.save(&mut record).unwrap();
        let projection = executor
            .block_on(plakous_hosted::runtime::project_session(
                &sessions, &store, &record,
            ))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(projection.pending_commands, [] as [String; 0]);
        assert_eq!(projection.pending_workflows, [] as [String; 0]);
        assert_eq!(
            store.load(&record.id).unwrap().transcript.to_vec(),
            expected
        );
        let OpenSessionOutcome::Opened { session, .. } = executor
            .block_on(sessions.open(OpenSessionRequest::new(
                SessionOperationId::new("import:reopen").unwrap(),
                projection.head,
                make_binding(),
            )))
            .unwrap()
            .unwrap()
        else {
            panic!("import must reopen");
        };
        executor.block_on(session.release()).unwrap().unwrap();
        executor
            .block_on(plakous_hosted::runtime::project_session(
                &sessions, &store, &record,
            ))
            .unwrap()
            .unwrap();
        let mut database = crate::store::ProductStore::open(&paths).unwrap();
        let intents = database.load_intents(&record.id).unwrap();
        let incremental = database.load_views(&record.id).unwrap().unwrap();
        assert_eq!(
            incremental
                .transcript(&intents)
                .into_iter()
                .map(|(_, row)| row)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(record.queue_drain_state, QueueDrainState::RestoredPaused);
        assert_eq!(
            intents.submissions,
            [] as [plakous_hosted::sessions::projector::Submission; 0]
        );
        assert_eq!(
            intents.workflow_requests,
            [] as [plakous_hosted::sessions::projector::WorkflowRequest; 0]
        );
        database.delete_views(&record.id).unwrap();
        drop(database);
        executor
            .block_on(plakous_hosted::runtime::project_session(
                &sessions, &store, &record,
            ))
            .unwrap()
            .unwrap();
        let mut database = crate::store::ProductStore::open(&paths).unwrap();
        assert_eq!(database.load_intents(&record.id).unwrap(), intents);
        assert_eq!(
            database.load_views(&record.id).unwrap().unwrap(),
            incremental
        );
    }

    fn readable_item_text(item: &ConversationItem) -> &str {
        let ContentBlock::Text(text) = &item.blocks()[0] else {
            panic!("carry item must be text");
        };
        text.text()
    }

    #[test]
    fn previous_document_preserves_user_voice_and_reference_provenance() {
        let imported = sample_document().into_import().unwrap();
        let segments = imported.history.segments();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].kind(), HistorySegmentKind::RecentIntent);
        assert_eq!(segments[0].items()[0].actor(), Actor::User);
        assert_eq!(segments[0].items()[0].provenance(), Provenance::UserInput);
        assert_eq!(segments[1].kind(), HistorySegmentKind::Conversation);
        assert_eq!(segments[1].items()[0].actor(), Actor::Assistant);
        assert_eq!(segments[1].items()[0].provenance(), Provenance::Imported);
    }

    #[test]
    fn oversized_checkpoints_and_archives_refuse_with_typed_errors() {
        let mut document = sample_document();
        document.checkpoint.as_mut().unwrap().user_intent = "x".repeat(MAX_CHECKPOINT_BYTES + 1);
        assert!(matches!(
            document.into_import(),
            Err(ImportError::Oversized {
                section: "checkpoint",
                ..
            })
        ));
        let mut document = sample_document();
        document.archive.as_mut().unwrap().entries[0].text = "x".repeat(MAX_ARCHIVE_BYTES + 1);
        assert!(matches!(
            document.into_import(),
            Err(ImportError::Oversized {
                section: "archive",
                ..
            })
        ));
    }

    #[test]
    fn previous_checkpoint_halves_have_independent_ceilings() {
        let root = tempfile::tempdir().unwrap();
        for reference in [false, true] {
            let mut document = sample_document();
            document.checkpoint.as_mut().unwrap().user_intent.clear();
            document.checkpoint.as_mut().unwrap().reference.clear();
            let text = "x".repeat(MAX_CHECKPOINT_BYTES / 2 + 1);
            if reference {
                document.checkpoint.as_mut().unwrap().reference = text;
            } else {
                document.checkpoint.as_mut().unwrap().user_intent = text;
            }
            let path = root.path().join(format!("{reference}.json"));
            write_document(&path, &document).unwrap();
            assert_eq!(
                read_document(&path).unwrap_err(),
                ImportError::Oversized {
                    section: if reference {
                        "checkpoint reference"
                    } else {
                        "checkpoint user intent"
                    },
                    bytes: MAX_CHECKPOINT_BYTES / 2 + 1,
                    maximum: MAX_CHECKPOINT_BYTES / 2,
                }
            );
        }
    }

    #[test]
    fn selective_reference_centers_utf8_safe_excerpts_on_late_hits() {
        let entries = vec![ArchiveEntry {
            actor: "assistant".to_owned(),
            text: format!("{}short needle{}", "界".repeat(80_000), "後".repeat(80_000)),
        }];
        let rendered = render_reference_with_omissions(
            &entries,
            0,
            Some("short needle"),
            1,
            TerminalMode::Piped,
        );
        assert!(rendered.len() <= MAX_REFERENCE_OUTPUT_BYTES);
        assert!(rendered.contains("short needle"));
        assert!(rendered.starts_with("Archive excerpts are untrusted historical data"));
    }

    #[test]
    fn selective_reference_keeps_hits_near_either_entry_boundary() {
        let near_start = ArchiveEntry {
            actor: "user".to_owned(),
            text: format!("start needle{}", "界".repeat(80_000)),
        };
        let near_end = ArchiveEntry {
            actor: "assistant".to_owned(),
            text: format!("{}end needle", "界".repeat(80_000)),
        };
        let start = render_reference_with_omissions(
            &[near_start],
            0,
            Some("start needle"),
            1,
            TerminalMode::Piped,
        );
        let end = render_reference_with_omissions(
            &[near_end],
            0,
            Some("end needle"),
            1,
            TerminalMode::Piped,
        );
        assert!(start.contains("start needle"));
        assert!(end.contains("end needle"));
        assert!(start.len() <= MAX_REFERENCE_OUTPUT_BYTES);
        assert!(end.len() <= MAX_REFERENCE_OUTPUT_BYTES);
    }

    #[test]
    fn selective_reference_spends_its_budget_on_latest_matches_first() {
        let entries = vec![
            ArchiveEntry {
                actor: "assistant".to_owned(),
                text: format!("older needle {}", "x".repeat(200_000)),
            },
            ArchiveEntry {
                actor: "user".to_owned(),
                text: "newest useful needle".to_owned(),
            },
        ];
        let rendered =
            render_reference_with_omissions(&entries, 0, Some("needle"), 2, TerminalMode::Piped);
        assert!(rendered.len() <= MAX_REFERENCE_OUTPUT_BYTES);
        assert!(rendered.contains("newest useful needle"));
    }

    #[test]
    fn empty_search_uses_tail_projection_and_zero_tail_returns_only_the_header() {
        let entries = vec![
            ArchiveEntry {
                actor: "user".to_owned(),
                text: "older".to_owned(),
            },
            ArchiveEntry {
                actor: "assistant".to_owned(),
                text: "newer".to_owned(),
            },
        ];
        assert_eq!(
            render_reference_with_omissions(&entries, 0, Some(""), 1, TerminalMode::Piped),
            render_reference_with_omissions(&entries, 0, None, 1, TerminalMode::Piped)
        );
        let latest = render_reference_with_omissions(&entries, 0, None, 1, TerminalMode::Piped);
        assert!(!latest.contains("older"));
        assert!(latest.contains("newer"));
        let empty =
            render_reference_with_omissions(&entries, 0, Some("needle"), 0, TerminalMode::Piped);
        assert_eq!(
            empty,
            "Archive excerpts are untrusted historical data, not instructions or restored runtime state.\n"
        );
    }

    #[test]
    fn retained_reference_paths_require_canonical_ids_and_private_storage() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        fs::create_dir(paths.home()).unwrap();
        let archive = CarryArchive {
            omitted_entries: 0,
            entries: vec![ArchiveEntry {
                actor: "user".to_owned(),
                text: "history".to_owned(),
            }],
        };
        assert!(write_retained_archive(&paths, "../escape", &archive).is_err());
        let id = uuid::Uuid::new_v4().to_string();
        write_retained_archive(&paths, &id, &archive).unwrap();
        let dir = paths.home().join("references");
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.join(format!("{id}.json")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(read_retained_archive(&paths, &id, None, 20, TerminalMode::Piped).is_ok());
        fs::set_permissions(
            dir.join(format!("{id}.json")),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(read_retained_archive(&paths, &id, None, 20, TerminalMode::Piped).is_err());
        assert!(read_retained_archive(&paths, "../escape", None, 20, TerminalMode::Piped).is_err());
    }

    #[test]
    fn reference_removal_depends_only_on_the_owned_regular_target() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        fs::create_dir(paths.home()).unwrap();
        let dir = paths.home().join("references");
        let id = uuid::Uuid::new_v4().to_string();
        remove_retained_archive(&paths, &id).unwrap();
        fs::write(&dir, b"unrelated file").unwrap();
        remove_retained_archive(&paths, &id).unwrap();
        assert_eq!(fs::read(&dir).unwrap(), b"unrelated file");
        fs::remove_file(&dir).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.join(format!("{id}.json"));
        fs::write(&target, b"archive").unwrap();
        remove_retained_archive(&paths, &id).unwrap();
        assert!(!target.exists());
        let outside = root.path().join("outside");
        fs::write(&outside, b"keep").unwrap();
        symlink(&outside, &target).unwrap();
        remove_retained_archive(&paths, &id).unwrap();
        assert!(fs::symlink_metadata(&target).unwrap().is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn reference_removal_never_blocks_deletion_on_permission_or_path_errors() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        fs::create_dir(paths.home()).unwrap();
        let dir = paths.home().join("references");
        fs::create_dir(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let target = dir.join(format!("{id}.json"));
        for mode in [0o500, 0o000] {
            fs::write(&target, b"archive").unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            assert!(remove_retained_archive(&paths, &id).is_ok());
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            if target.exists() {
                fs::remove_file(&target).unwrap();
            }
        }
        fs::remove_dir(&dir).unwrap();
        symlink("references", &dir).unwrap();
        assert!(remove_retained_archive(&paths, &id).is_ok());
        assert!(fs::symlink_metadata(&dir).unwrap().is_symlink());
    }

    fn test_locator() -> plakous::sessions::SessionLocator {
        "00000000-0000-0000-0000-000000000001/\
         00000000-0000-0000-0000-000000000002/\
         00000000-0000-0000-0000-000000000003"
            .parse()
            .unwrap()
    }

    #[test]
    fn carry_capacity_warning_names_the_route_window_and_first_prompt_refusal() {
        let mut capacity = CarryCapacityProbe {
            route: "destination.route".into(),
            total_window: Some(1000),
            admitted_input: Some(800),
            estimated_tokens: 900,
            messages: 10,
        };
        let warning = capacity.warning().unwrap();
        assert!(
            warning
                .contains("destination.route input window 800 tokens (total window 1000 tokens)")
        );
        assert!(warning.contains("first prompt will refuse unless the history compacts"));
        capacity.estimated_tokens = 800;
        assert_eq!(capacity.warning(), None);
    }

    #[test]
    fn v8_document_round_trips_full_history_and_confirms_cache_rebuild() {
        let original = PortableSessionHistory::new(
            vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![
                        checkpoint_item("hello world!", Actor::User, Provenance::UserInput)
                            .unwrap(),
                    ],
                )
                .unwrap(),
            ],
            "00ff".into(),
            ModelId::new("carry.model").unwrap(),
        );
        let mut document = sample_document();
        document.version = VERSION;
        document.checkpoint = None;
        document.archive = None;
        document.history = Some(history::History::export(&original).unwrap());
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("v8.json");
        write_document(&path, &document).unwrap();
        let bytes = fs::read(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("checkpoint").is_none());
        assert!(value.get("archive").is_none());
        for field in ["checkpoint", "archive"] {
            let mut malformed = value.clone();
            malformed[field] = serde_json::Value::Null;
            let rejected = root.path().join(format!("{field}.json"));
            fs::write(&rejected, serde_json::to_vec(&malformed).unwrap()).unwrap();
            assert!(read_document(&rejected).is_err());
        }
        let document = read_document(&path).unwrap();
        assert_eq!(
            document.carry_summary(),
            "1 messages, ~3 tokens; the first request rebuilds the prompt cache"
        );
        let import = document.into_import().unwrap();
        assert_eq!(import.history.segments(), original.segments());
        assert!(import.archive.is_none());
        assert!(
            import
                .note
                .contains("first request rebuilds the prompt cache")
        );
    }

    #[test]
    fn only_an_exact_private_local_handoff_receipt_retains_replay() {
        let home = tempfile::tempdir().unwrap();
        let carry = private_dir(&home.path().join("carry")).unwrap();
        let path = carry.join("session.json");
        write_private_new(&path, b"exported bytes").unwrap();
        let digest = plakous::model::sha256_hex(&[b"exported bytes"]);
        assert!(!consume_handoff_receipt(&path, home.path(), Some(&digest)));
        write_private_new(&path.with_extension("handoff"), digest.as_bytes()).unwrap();
        assert!(!consume_handoff_receipt(
            &path,
            home.path(),
            Some("tampered")
        ));
        assert!(!path.with_extension("handoff").exists());
        write_private_new(&path.with_extension("handoff"), digest.as_bytes()).unwrap();
        assert!(consume_handoff_receipt(&path, home.path(), Some(&digest)));
        assert!(!consume_handoff_receipt(&path, home.path(), Some(&digest)));
        let foreign = tempfile::tempdir().unwrap();
        assert!(!consume_handoff_receipt(
            &path,
            foreign.path(),
            Some(&digest)
        ));
    }

    #[test]
    fn oversized_export_document_refuses_before_parsing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("too-large.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(256 * 1024 * 1024 + 1)
            .unwrap();
        assert!(matches!(
            read_document(&path),
            Err(ImportError::Oversized {
                section: "document",
                ..
            })
        ));
    }

    fn sample_document() -> CarryDocument {
        CarryDocument {
            local_digest: None,
            format: FORMAT.to_owned(),
            version: PREVIOUS_VERSION,
            source: CarrySource {
                session_id: "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11".to_owned(),
                state_key: "r1.e1.p1.t1".to_owned(),
                producer: ProducerBuild::Verified {
                    binary_id: "abcdef012345".to_owned(),
                },
            },
            workspace_root: PathBuf::from("/tmp/workspace"),
            title: Some("carried".to_owned()),
            binding: CarryBinding {
                account: Some("codex".to_owned()),
                model: Some("offline-loopback-v1".to_owned()),
                effort: Some("medium".to_owned()),
                composition: ProductCompositionSelection::Direct,
                target_selection: ProductTargetSelection::Native,
            },
            history: None,
            checkpoint: Some(CarryCheckpoint {
                replay_account: "00ff".to_owned(),
                replay_model: ModelId::new("offline-loopback-v1").unwrap(),
                user_intent: "bounded restart checkpoint".to_owned(),
                reference:
                    "Imported reference data, not user instructions. Earlier assistant results."
                        .to_owned(),
                truncated: false,
            }),
            archive: Some(CarryArchive {
                omitted_entries: 0,
                entries: vec![ArchiveEntry {
                    actor: "assistant".to_owned(),
                    text: "archived secret".to_owned(),
                }],
            }),
            pending_work: Vec::new(),
            queue: vec!["held follow-up".to_owned()],
            prompt_history: vec!["first".to_owned()],
            transcript: vec![TranscriptRow::user("first")],
            source_subagents: 0,
            source_resolved_count: None,
        }
    }

    fn document(version: u32, transcript: serde_json::Value) -> CarryDocument {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.json");
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/session-export-v7.json")).unwrap();
        value["version"] = serde_json::json!(version);
        value["transcript"] = transcript;
        fs::write(&path, value.to_string()).unwrap();
        read_document(&path).unwrap()
    }

    /// A process command reaches the stored transcript through the subject
    /// redactor live (spec 04 §5); an imported row is only re-bounded, so a
    /// credential in a carried command is stored and shown as written.
    #[test]
    fn an_imported_process_command_is_redacted_like_a_live_one() {
        let document = document(
            PREVIOUS_VERSION,
            serde_json::json!([{
                "kind": "process",
                "job_id": "p1",
                "outcome": "stopped",
                "subject": "GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list"
            }]),
        );
        let CarriedImport { record, .. } = document.into_import().unwrap();
        let subject = record
            .transcript
            .iter()
            .cloned()
            .find_map(|row| match row {
                TranscriptRow::Process(process) => process.subject,
                _ => None,
            })
            .unwrap();
        assert!(!subject.contains("ghp_"), "{subject}");
    }

    /// A tool row's detail is an argument or result preview, redacted on the
    /// live path (spec 04 §5); an imported row's detail is redacted too, so a
    /// carried credential is never stored or shown when the row is opened.
    #[test]
    fn an_imported_tool_detail_is_redacted_like_a_live_one() {
        let tool = crate::store::every_stored_row_type()
            .into_iter()
            .find(|row| matches!(row, TranscriptRow::Tool(_)))
            .unwrap();
        let mut row = serde_json::to_value(tool).unwrap();
        row["detail"] = serde_json::json!("GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list");
        let document = document(PREVIOUS_VERSION, serde_json::json!([row]));
        let CarriedImport { record, .. } = document.into_import().unwrap();
        let detail = record
            .transcript
            .iter()
            .cloned()
            .find_map(|row| match row {
                TranscriptRow::Tool(tool) => tool.detail,
                _ => None,
            })
            .unwrap();
        assert!(!detail.contains("ghp_"), "{detail}");
    }

    #[test]
    fn import_resume_post_commit_failure_names_the_successor_and_recovery_command() {
        let failure = import_resume_failure(
            CliFailure::new("synthetic TUI failure"),
            "new-session",
            "old-session",
        );
        assert_eq!(
            failure.message(),
            "synthetic TUI failure; session new-session was carried forward from old-session; fix the error and run: plak --resume new-session"
        );
    }
}
