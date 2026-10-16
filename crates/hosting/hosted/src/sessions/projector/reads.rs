//! The bounded facade reads the projector consumes (spec 01 §5.11), as owned
//! values. `plan_reads` names every read a record needs from the record
//! alone, `load_reads` performs them through `feed_items`, `feed_call` and
//! `feed_terminal`, and the pure projector looks them up.

use std::collections::BTreeMap;

use plakous::model::{Actor, BranchId, CallId, CheckpointId, EntryId, ReasoningVisibility, TurnId};
use plakous::sessions::feed::{
    FeedCall, FeedCallOutcome, FeedCallRequest, FeedCallTarget, FeedChange, FeedFact,
    FeedItemsOutcome, FeedItemsRequest, FeedOperationKind, FeedOperationState, FeedRecord,
    FeedTerminalOutcome, FeedTerminalRequest, FeedTurnOutcome,
};
use plakous::sessions::{
    AgentSessions, AgentSessionsError, SessionTranscriptItem, SessionTranscriptPart,
};

/// What a call read names.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CallTarget {
    /// An operation, as an `OperationAdvanced` or `CommandAccepted` names it.
    Operation(u64),
    /// A call in history, by its call id.
    Call(CallId),
}

/// One bounded read at a record's checkpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ReadRequest {
    /// `feed_items` for a `HistoryAppended` range.
    Items {
        /// The record's branch.
        branch: BranchId,
        /// The record's checkpoint.
        checkpoint: CheckpointId,
        /// First item ordinal.
        first_ordinal: u64,
        /// Item count.
        count: u64,
        /// First entry identity: named on the first chunk of a range only.
        first_entry: Option<EntryId>,
        /// Last entry identity: named on the last chunk of a range only.
        last_entry: Option<EntryId>,
    },
    /// `feed_call`.
    Call {
        /// The record's branch.
        branch: BranchId,
        /// The record's checkpoint.
        checkpoint: CheckpointId,
        /// The call.
        target: CallTarget,
    },
    /// `feed_terminal`.
    Terminal {
        /// The record's branch.
        branch: BranchId,
        /// The record's checkpoint.
        checkpoint: CheckpointId,
        /// The root turn.
        turn_id: TurnId,
    },
}

/// Who wrote an item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadActor {
    /// The operator.
    User,
    /// The model.
    Assistant,
    /// Anything else.
    Other,
}

/// One presentation part of an item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadPart {
    /// Prose.
    Text(String),
    /// The transcript chip of an inline image.
    Image(String),
    /// Reasoning with operator-visible display text.
    Reasoning(String),
    /// A tool call block.
    Call {
        /// Call id.
        call_id: String,
        /// Canonical name.
        canonical: String,
        /// The name the model called.
        exposed: String,
        /// The arguments preview.
        arguments: Option<String>,
        /// The outer local-composition call owning it.
        parent_call_id: Option<String>,
        /// The local-composition cell owning it.
        cell_id: Option<String>,
        /// The bounded redacted subject of the call block: the subject of a
        /// call no operation backs (rejected before preparation).
        subject: Option<String>,
    },
    /// A tool result block in the same item: a provider-hosted call's result,
    /// committed with its call in the response segment.
    Result {
        /// The call it answers.
        call_id: String,
        /// The bounded redacted result.
        result: ResultRead,
    },
    /// A compaction summary.
    Compaction,
}

/// One history item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadItem {
    /// Its author.
    pub actor: ReadActor,
    /// The harness wrote it, not the operator.
    pub harness_authored: bool,
    /// Its parts.
    pub parts: Vec<ReadPart>,
}

/// A committed tool result, whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultRead {
    /// It succeeded.
    pub succeeded: bool,
    /// Its text parts, joined.
    pub text: String,
}

/// A process's committed terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobEnd {
    /// It was stopped.
    pub stopped: bool,
    /// Its exit code.
    pub exit_code: Option<i32>,
}

/// One call read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallRead {
    /// A history call and its committed result.
    Tool {
        /// Canonical name.
        canonical: String,
        /// The name the model called.
        exposed: String,
        /// Arguments as JSON text.
        arguments: Option<String>,
        /// The committed result.
        result: Option<ResultRead>,
    },
    /// A Code Cell's nested call.
    Nested {
        /// Its tool name.
        name: String,
        /// Its arguments as JSON text.
        arguments: Option<String>,
    },
    /// A process session.
    Job {
        /// Its job number.
        number: Option<u32>,
        /// Its command line.
        command: Option<String>,
        /// Its terminal.
        end: Option<JobEnd>,
    },
    /// A steer's text.
    Steer(String),
    /// Nothing the read could name is retained.
    NotRetained,
}

/// One root terminal read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalRead {
    /// The terminal's operator-facing failure text.
    Failed(String),
    /// A terminal without a failure.
    Other,
    /// Not retained.
    NotRetained,
}

/// One read's value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadValue {
    /// Items, or `None` when not retained.
    Items(Option<Vec<ReadItem>>),
    /// A call.
    Call(CallRead),
    /// A terminal.
    Terminal(TerminalRead),
}

/// The reads a projector run may look up.
pub trait ProjectorReads {
    /// The value of a read, when it was performed.
    fn read(&self, request: &ReadRequest) -> Option<&ReadValue>;
}

/// Reads performed ahead of a projector run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadCache(BTreeMap<ReadRequest, ReadValue>);

impl ReadCache {
    /// Records a read's value.
    pub fn insert(&mut self, request: ReadRequest, value: ReadValue) {
        self.0.insert(request, value);
    }
}

impl ProjectorReads for ReadCache {
    fn read(&self, request: &ReadRequest) -> Option<&ReadValue> {
        self.0.get(request)
    }
}

/// The checkpoint a record names, when its change names one.
#[must_use]
pub fn record_checkpoint(record: &FeedRecord) -> Option<CheckpointId> {
    match record.header().change {
        FeedChange::Created { genesis, .. } => Some(genesis),
        FeedChange::Advanced { checkpoint, .. } => Some(checkpoint),
        FeedChange::Branched { target, .. } => Some(target),
        _ => None,
    }
}

/// Whether a history segment derives transcript rows: a steering segment's
/// row derives from its `CommandAccepted` and a compaction's from its
/// workflow facts. A reasoning replay unit derives rows: a response whose
/// reasoning must replay is committed whole as one, prose included.
#[must_use]
pub const fn derives_rows(segment: plakous::model::HistorySegmentKind) -> bool {
    matches!(
        segment,
        plakous::model::HistorySegmentKind::Conversation
            | plakous::model::HistorySegmentKind::RecentIntent
            | plakous::model::HistorySegmentKind::ToolExchange
            | plakous::model::HistorySegmentKind::ReasoningReplay
    )
}

/// The call read an `OperationAdvanced` fact needs, by the same rule the
/// projector applies: an effect's call block at dispatch (a direct effect's
/// from its operation record's request until its result commits, a Code
/// Cell's nested call from its operation record, which never reaches
/// history), and a process session's job when it ends. A direct effect's
/// result is read through the `HistoryAppended` that commits it.
#[must_use]
pub fn operation_read(
    operation_id: u64,
    kind: &FeedOperationKind,
    state: FeedOperationState,
) -> Option<CallTarget> {
    match (kind, state) {
        (FeedOperationKind::Effect { .. }, FeedOperationState::Dispatched)
        | (
            FeedOperationKind::ProcessSession { .. },
            FeedOperationState::Completed | FeedOperationState::OutcomeUnknown,
        ) => Some(CallTarget::Operation(operation_id)),
        _ => None,
    }
}

/// The most items one `feed_items` read names: the facade's bound
/// (spec 01 §5.11).
const FEED_ITEMS_CHUNK: u64 = 4_096;

/// The `feed_items` reads of one `HistoryAppended` range, in order: chunks of
/// at most [`FEED_ITEMS_CHUNK`] items at the record's checkpoint. Only the
/// first chunk names the first entry and only the last names the last; the
/// chunks between are checked by ordinal continuity (spec 01 §5.11).
#[must_use]
pub(super) fn item_reads(
    branch: BranchId,
    checkpoint: CheckpointId,
    first_ordinal: u64,
    count: u64,
    first_entry: EntryId,
    last_entry: EntryId,
) -> Vec<ReadRequest> {
    let end = first_ordinal.saturating_add(count);
    let mut chunks = Vec::new();
    let mut start = first_ordinal;
    while start < end {
        let chunk = (end - start).min(FEED_ITEMS_CHUNK);
        chunks.push(ReadRequest::Items {
            branch,
            checkpoint,
            first_ordinal: start,
            count: chunk,
            first_entry: (start == first_ordinal).then_some(first_entry),
            last_entry: (start + chunk == end).then_some(last_entry),
        });
        start += chunk;
    }
    chunks
}

/// Every read the projector may make for `record`, from the record alone.
#[must_use]
pub fn plan_reads(record: &FeedRecord) -> Vec<ReadRequest> {
    let Some(checkpoint) = record_checkpoint(record) else {
        return Vec::new();
    };
    let branch = record.header().branch_id;
    let call = |target| ReadRequest::Call {
        branch,
        checkpoint,
        target,
    };
    let mut reads = Vec::new();
    for fact in record.facts() {
        match fact {
            FeedFact::CommandAccepted {
                kind: plakous::sessions::feed::FeedCommandKind::Steer,
                operation_id: Some(operation_id),
                ..
            } => reads.push(call(CallTarget::Operation(*operation_id))),
            FeedFact::HistoryAppended {
                segment,
                first_ordinal,
                count,
                first_entry,
                last_entry,
                tool_results,
                ..
            } => {
                if derives_rows(*segment) {
                    reads.extend(item_reads(
                        branch,
                        checkpoint,
                        *first_ordinal,
                        *count,
                        *first_entry,
                        *last_entry,
                    ));
                }
                reads.extend(
                    tool_results
                        .iter()
                        .map(|call_id| call(CallTarget::Call(*call_id))),
                );
            }
            FeedFact::OperationAdvanced {
                operation_id,
                kind,
                state,
                ..
            } => reads.extend(operation_read(*operation_id, kind, *state).map(call)),
            FeedFact::TurnTerminal {
                turn_id,
                outcome: FeedTurnOutcome::Failed { .. },
                ..
            } => reads.push(ReadRequest::Terminal {
                branch,
                checkpoint,
                turn_id: *turn_id,
            }),
            _ => {}
        }
    }
    reads.sort();
    reads.dedup();
    reads
}

/// Performs `requests` for `record`'s session through the facade reads.
///
/// # Errors
///
/// Returns the facade's refusal of any read.
pub async fn load_reads(
    sessions: &AgentSessions,
    session: plakous::model::SessionId,
    requests: impl IntoIterator<Item = ReadRequest>,
    cache: &mut ReadCache,
) -> Result<(), AgentSessionsError> {
    for request in requests {
        if cache.read(&request).is_some() {
            continue;
        }
        let value =
            match request {
                ReadRequest::Items {
                    branch,
                    checkpoint,
                    first_ordinal,
                    count,
                    first_entry,
                    last_entry,
                } => ReadValue::Items(
                    match sessions
                        .feed_items(FeedItemsRequest {
                            session,
                            branch,
                            checkpoint,
                            first_ordinal,
                            count,
                            first_entry,
                            last_entry,
                        })
                        .await?
                    {
                        FeedItemsOutcome::Items { items, .. } => {
                            Some(items.iter().map(read_item).collect())
                        }
                        _ => None,
                    },
                ),
                ReadRequest::Call {
                    branch,
                    checkpoint,
                    target,
                } => ReadValue::Call(
                    match sessions
                        .feed_call(FeedCallRequest {
                            session,
                            branch,
                            checkpoint,
                            call: match target {
                                CallTarget::Operation(operation) => {
                                    FeedCallTarget::Operation(operation)
                                }
                                CallTarget::Call(call_id) => FeedCallTarget::Call(call_id),
                            },
                        })
                        .await?
                    {
                        FeedCallOutcome::Call { call, .. } => call_read(call),
                        _ => CallRead::NotRetained,
                    },
                ),
                ReadRequest::Terminal {
                    branch,
                    checkpoint,
                    turn_id,
                } => ReadValue::Terminal(
                    match sessions
                        .feed_terminal(FeedTerminalRequest {
                            session,
                            branch,
                            checkpoint,
                            turn_id,
                        })
                        .await?
                    {
                        FeedTerminalOutcome::Terminal(terminal) => terminal
                            .result()
                            .error()
                            .map_or(TerminalRead::Other, |error| {
                                TerminalRead::Failed(
                                    crate::runtime::events::operator_facing_agent_error(
                                        "agent turn failed",
                                        error,
                                    ),
                                )
                            }),
                        _ => TerminalRead::NotRetained,
                    },
                ),
            };
        cache.insert(request, value);
    }
    Ok(())
}

/// Read-only SQLite twin of [`load_reads`], using the same presentation
/// conversions and bounded read plan without constructing runtime ownership.
///
/// # Errors
/// Returns the facade's typed inspection refusal.
pub fn load_reads_sqlite(
    path: &std::path::Path,
    session: plakous::model::SessionId,
    requests: impl IntoIterator<Item = ReadRequest>,
    cache: &mut ReadCache,
) -> Result<(), AgentSessionsError> {
    for request in requests {
        if cache.read(&request).is_some() {
            continue;
        }
        let value =
            match request {
                ReadRequest::Items {
                    branch,
                    checkpoint,
                    first_ordinal,
                    count,
                    first_entry,
                    last_entry,
                } => ReadValue::Items(
                    match AgentSessions::inspect_feed_items_sqlite(
                        path,
                        &FeedItemsRequest {
                            session,
                            branch,
                            checkpoint,
                            first_ordinal,
                            count,
                            first_entry,
                            last_entry,
                        },
                    )? {
                        FeedItemsOutcome::Items { items, .. } => {
                            Some(items.iter().map(read_item).collect())
                        }
                        _ => None,
                    },
                ),
                ReadRequest::Call {
                    branch,
                    checkpoint,
                    target,
                } => ReadValue::Call(
                    match AgentSessions::inspect_feed_call_sqlite(
                        path,
                        &FeedCallRequest {
                            session,
                            branch,
                            checkpoint,
                            call: match target {
                                CallTarget::Operation(operation) => {
                                    FeedCallTarget::Operation(operation)
                                }
                                CallTarget::Call(call_id) => FeedCallTarget::Call(call_id),
                            },
                        },
                    )? {
                        FeedCallOutcome::Call { call, .. } => call_read(call),
                        _ => CallRead::NotRetained,
                    },
                ),
                ReadRequest::Terminal {
                    branch,
                    checkpoint,
                    turn_id,
                } => ReadValue::Terminal(
                    match AgentSessions::inspect_feed_terminal_sqlite(
                        path,
                        &FeedTerminalRequest {
                            session,
                            branch,
                            checkpoint,
                            turn_id,
                        },
                    )? {
                        FeedTerminalOutcome::Terminal(terminal) => terminal
                            .result()
                            .error()
                            .map_or(TerminalRead::Other, |error| {
                                TerminalRead::Failed(
                                    crate::runtime::events::operator_facing_agent_error(
                                        "agent turn failed",
                                        error,
                                    ),
                                )
                            }),
                        _ => TerminalRead::NotRetained,
                    },
                ),
            };
        cache.insert(request, value);
    }
    Ok(())
}

/// One history item as the projector reads it.
#[must_use]
pub fn read_item(item: &SessionTranscriptItem) -> ReadItem {
    ReadItem {
        actor: match item.actor() {
            Actor::User => ReadActor::User,
            Actor::Assistant => ReadActor::Assistant,
            _ => ReadActor::Other,
        },
        harness_authored: item.is_harness_authored(),
        parts: item.parts().iter().filter_map(read_part).collect(),
    }
}

fn read_part(part: &SessionTranscriptPart) -> Option<ReadPart> {
    match part {
        SessionTranscriptPart::Text(text) => Some(ReadPart::Text(text.clone())),
        SessionTranscriptPart::Reasoning { visibility, text } => matches!(
            visibility,
            ReasoningVisibility::Visible | ReasoningVisibility::Summary
        )
        .then(|| ReadPart::Reasoning(text.clone())),
        SessionTranscriptPart::ToolCall {
            call_id,
            canonical_name,
            exposed_name,
            observation,
            lineage,
        } => Some(ReadPart::Call {
            call_id: call_id.to_string(),
            canonical: canonical_name.as_str().to_owned(),
            exposed: exposed_name.as_str().to_owned(),
            arguments: Some(observation.arguments_preview().to_owned()),
            parent_call_id: lineage.parent_call_id().map(|parent| parent.to_string()),
            cell_id: lineage.cell_id().map(str::to_owned),
            subject: observation.subject().map(str::to_owned),
        }),
        SessionTranscriptPart::ToolResult {
            call_id,
            observation,
            ..
        } => Some(ReadPart::Result {
            call_id: call_id.to_string(),
            result: ResultRead {
                succeeded: observation.succeeded(),
                text: observation.preview().to_owned(),
            },
        }),
        SessionTranscriptPart::Compaction => Some(ReadPart::Compaction),
        SessionTranscriptPart::Image {
            media_type,
            byte_len,
            dimensions,
        } => Some(ReadPart::Image(crate::sessions::image_chip(
            media_type,
            *byte_len,
            *dimensions,
        ))),
        _ => None,
    }
}

fn call_read(call: FeedCall) -> CallRead {
    match call {
        FeedCall::Tool { call, result } => CallRead::Tool {
            canonical: call.canonical_name().as_str().to_owned(),
            exposed: call.exposed_name().as_str().to_owned(),
            arguments: Some(call.raw_arguments().to_owned()),
            result: result.map(|result| ResultRead {
                succeeded: result.kind() == plakous::model::ToolResultKind::Success,
                text: result
                    .parts()
                    .iter()
                    .filter_map(|part| match part {
                        plakous::model::ToolResultPart::Text(text) => Some(text.text()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            }),
        },
        FeedCall::Nested {
            name, arguments, ..
        } => CallRead::Nested {
            name: name.as_str().to_owned(),
            arguments,
        },
        FeedCall::Job {
            number,
            command,
            terminal,
        } => CallRead::Job {
            number,
            command,
            end: terminal.map(|terminal| JobEnd {
                stopped: terminal.stopped(),
                exit_code: terminal.exit_code(),
            }),
        },
        FeedCall::Steer { text } => CallRead::Steer(text),
        _ => CallRead::NotRetained,
    }
}
