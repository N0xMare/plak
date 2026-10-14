//! The change feed's public surface (spec 01 §5.11).
//!
//! Each runtime session exposes one ordered, read-only feed of every mutation
//! the store accepted for it. A consumer pages records after its cursor,
//! waits on a coalescing tail notification, and reads the content a record
//! names through bounded reads at the record's checkpoint. Nothing here
//! writes: there is no feed write path.

mod notify;

use std::num::NonZeroUsize;
use std::sync::Arc;

use plakous_agent::internal::facade_support::{
    FeedOperationCall, checkpoint_completed_terminal_source, checkpoint_operation_call,
    checkpoint_retained_turn_terminal, checkpoint_turn_terminal,
};
use plakous_agent::internal::{FeedPage as StoreFeedPage, SessionStoreError, StoredCheckpoint};
use plakous_agent::{AbandonedTurnReport, TurnResult};
use plakous_model::{
    BranchId, CallId, CheckpointId, ContentBlock, ConversationItem, EntryId, SessionId,
    ToolCallBlock, ToolResultBlock, TurnId,
};

use super::transcript::SessionTranscriptItem;
use super::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, RuntimeCheckpoint, SessionLocator,
    SessionStore, runtime_error, store_error, transcript,
};
#[cfg(feature = "sessions-sqlite")]
use super::{Path, SqliteSessionStore};
use notify::FeedChanges;
pub(super) use notify::{FeedNotifier, NotifyingStore};
pub use plakous_agent::internal::{
    FeedBranchTransition, FeedCallerLineage, FeedCancelCause, FeedChange, FeedCommandKind,
    FeedCompactPass, FeedCompactReason, FeedContextLimit, FeedFact, FeedHeadKind, FeedHeader,
    FeedLocator, FeedOperationKind, FeedOperationState, FeedRecord, FeedSeq, FeedTurnOutcome,
    FeedWorkflowKind, FeedWorkflowOutcome, MAX_FEED_PAGE_BYTES, MAX_FEED_RECORDS_PER_SESSION,
};

/// Most records one [`AgentSessions::feed_page`] read returns.
pub const MAX_FEED_PAGE_RECORDS: usize = 256;
/// Most items one [`AgentSessions::feed_items`] read returns.
pub const MAX_FEED_ITEMS: u64 = 4_096;

/// A consumer's position in one session's feed: the last applied sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FeedCursor {
    session_id: SessionId,
    applied: FeedSeq,
}

impl FeedCursor {
    /// The cursor before the first record of `session_id`'s feed.
    #[must_use]
    pub const fn start(session_id: SessionId) -> Self {
        Self::new(session_id, FeedSeq::ZERO)
    }

    /// The cursor after `applied`.
    #[must_use]
    pub const fn new(session_id: SessionId, applied: FeedSeq) -> Self {
        Self {
            session_id,
            applied,
        }
    }

    /// Returns the runtime session.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the last applied sequence.
    #[must_use]
    pub const fn applied(self) -> FeedSeq {
        self.applied
    }

    /// The cursor after `record`, when it belongs to this session.
    #[must_use]
    pub fn advanced_past(self, record: &FeedRecord) -> Self {
        if record.header().session_id != self.session_id || record.header().seq <= self.applied {
            return self;
        }
        Self::new(self.session_id, record.header().seq)
    }
}

/// One bounded read of a session's feed after a cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedPageRequest {
    /// The consumer's cursor.
    pub cursor: FeedCursor,
    /// Most records to return, at most [`MAX_FEED_PAGE_RECORDS`].
    pub max_records: NonZeroUsize,
}

impl FeedPageRequest {
    /// Reads up to [`MAX_FEED_PAGE_RECORDS`] after `cursor`.
    #[must_use]
    pub const fn after(cursor: FeedCursor) -> Self {
        Self {
            cursor,
            max_records: match NonZeroUsize::new(MAX_FEED_PAGE_RECORDS) {
                Some(max) => max,
                None => NonZeroUsize::MIN,
            },
        }
    }

    fn validate(self) -> Result<Self, AgentSessionsError> {
        if self.max_records.get() > MAX_FEED_PAGE_RECORDS {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::InvalidInput,
                "a feed page reads at most 256 records",
            ));
        }
        Ok(self)
    }
}

/// One page of a session's feed.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FeedPage {
    /// Records after the cursor, oldest first, with the retained window
    /// observed in the same read. Empty at the tail.
    Records {
        /// The records.
        records: Vec<FeedRecord>,
        /// The oldest retained sequence.
        retained_from: FeedSeq,
        /// The newest assigned sequence.
        tail: FeedSeq,
    },
    /// The cursor is below the retained window. Nothing older is recoverable:
    /// rebuild from a current head and its history, then resume at `tail`.
    BehindRetention {
        /// The oldest retained sequence.
        retained_from: FeedSeq,
        /// The newest assigned sequence.
        tail: FeedSeq,
        /// The session's current branch heads.
        heads: Vec<SessionLocator>,
    },
}

impl FeedPage {
    fn from_store(page: StoreFeedPage) -> Self {
        match page {
            StoreFeedPage::Records { records, window } => Self::Records {
                records,
                retained_from: window.retained_from(),
                tail: window.tail(),
            },
            StoreFeedPage::BehindRetention { window, heads } => Self::BehindRetention {
                retained_from: window.retained_from(),
                tail: window.tail(),
                heads: heads
                    .into_iter()
                    .map(|head| {
                        SessionLocator::new(
                            head.session().session_id(),
                            head.session().branch_id(),
                            head.checkpoint().checkpoint_id(),
                        )
                    })
                    .collect(),
            },
        }
    }
}

/// An in-process, coalescing notification of one session's feed tail.
///
/// It carries no records and never backpressures the runtime: a consumer
/// reads pages after [`Self::changed`] resolves.
pub struct FeedSubscription {
    session_id: SessionId,
    store: Arc<dyn SessionStore>,
    changes: FeedChanges,
    seen: FeedSeq,
}

impl std::fmt::Debug for FeedSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FeedSubscription")
            .field("session_id", &self.session_id)
            .field("seen", &self.seen)
            .finish_non_exhaustive()
    }
}

impl FeedSubscription {
    /// Returns the session this subscription follows.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Returns the newest tail this subscription has reported (the tail at
    /// subscription before the first change).
    #[must_use]
    pub const fn seen(&self) -> FeedSeq {
        self.seen
    }

    /// Resolves with the newest tail once the store acknowledged a mutation
    /// that moved it past [`Self::seen`]. Changes coalesce: several commits
    /// may resolve one call.
    ///
    /// # Errors
    ///
    /// Returns a typed lookup refusal once the session is gone, or a backend
    /// failure.
    pub async fn changed(&mut self) -> Result<FeedSeq, AgentSessionsError> {
        loop {
            self.changes.changed().await;
            let tail = self
                .store
                .feed_tail(self.session_id)
                .await
                .map_err(|error| store_error(error).for_session(self.session_id))?
                .tail();
            if tail > self.seen {
                self.seen = tail;
                return Ok(tail);
            }
        }
    }
}

/// One `HistoryAppended` range to read at the record's checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedItemsRequest {
    /// Runtime session.
    pub session: SessionId,
    /// Branch of the record.
    pub branch: BranchId,
    /// The record's checkpoint.
    pub checkpoint: CheckpointId,
    /// Ordinal of the first item in that checkpoint's canonical history.
    pub first_ordinal: u64,
    /// Item count, at most [`MAX_FEED_ITEMS`].
    pub count: u64,
    /// Entry identity of the first item, checked when named. A read without
    /// it is answered only from the named checkpoint itself: a reader that
    /// splits a longer `HistoryAppended` range names it on the first chunk
    /// only, and its other chunks are checked by ordinal continuity against
    /// that checkpoint.
    pub first_entry: Option<EntryId>,
    /// Entry identity of the last item, checked when named (on the last
    /// chunk of a split range).
    pub last_entry: Option<EntryId>,
}

/// The items a `HistoryAppended` fact names.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FeedItemsOutcome {
    /// The items as transcript presentation parts, oldest first.
    Items {
        /// The checkpoint they were read at: the named one, or the branch
        /// head when the named one was pruned.
        checkpoint: CheckpointId,
        /// The items.
        items: Vec<SessionTranscriptItem>,
    },
    /// The named checkpoint was pruned and a compaction removed the items
    /// since; nothing is guessed.
    NotRetained,
}

/// What a [`FeedCallRequest`] names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FeedCallTarget {
    /// An operation, as an `OperationAdvanced` fact names it.
    Operation(u64),
    /// A call rejected before preparation, by its call id.
    Call(CallId),
}

/// One call to read at a record's checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedCallRequest {
    /// Runtime session.
    pub session: SessionId,
    /// Branch of the record.
    pub branch: BranchId,
    /// The record's checkpoint.
    pub checkpoint: CheckpointId,
    /// The call.
    pub call: FeedCallTarget,
}

/// A process's committed terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedJobTerminal {
    stopped: bool,
    exit_code: Option<i32>,
}

impl FeedJobTerminal {
    /// Whether the process was stopped rather than exiting on its own.
    #[must_use]
    pub const fn stopped(self) -> bool {
        self.stopped
    }

    /// The natural exit code, when reported.
    #[must_use]
    pub const fn exit_code(self) -> Option<i32> {
        self.exit_code
    }
}

/// One call as committed, returned whole: never preview-clipped or
/// redacted. The consumer's row rules bound and redact what it persists.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum FeedCall {
    /// An effect or Code Cell call and, once committed, its result.
    Tool {
        /// The committed call block (canonical and exposed names, arguments).
        call: Box<ToolCallBlock>,
        /// The committed result, absent until it is committed.
        result: Option<Box<ToolResultBlock>>,
    },
    /// A Code Cell's nested call, as its operation record holds it.
    Nested {
        /// The nested call identity.
        call_id: CallId,
        /// The nested tool name.
        name: plakous_model::ToolName,
        /// The nested call's arguments, as JSON text.
        arguments: Option<String>,
    },
    /// A process session.
    Job {
        /// The job number, when the ledger retains its row.
        number: Option<u32>,
        /// The command line, when the ledger retains its row.
        command: Option<String>,
        /// Its terminal, once committed.
        terminal: Option<FeedJobTerminal>,
    },
    /// A steer command's text.
    Steer {
        /// The steering text as submitted.
        text: String,
    },
}

/// One call read.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum FeedCallOutcome {
    /// The call, read at `checkpoint`.
    Call {
        /// The named checkpoint, or the branch head when it was pruned.
        checkpoint: CheckpointId,
        /// The call.
        call: FeedCall,
    },
    /// The named checkpoint was pruned and the call is no longer retained.
    NotRetained,
}

/// One root turn's terminal record to read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedTerminalRequest {
    /// Runtime session.
    pub session: SessionId,
    /// Branch of the record.
    pub branch: BranchId,
    /// The record's checkpoint.
    pub checkpoint: CheckpointId,
    /// The root turn.
    pub turn_id: TurnId,
}

/// A root turn's committed terminal record.
#[derive(Clone, Debug)]
pub struct FeedTerminal {
    checkpoint: CheckpointId,
    result: TurnResult,
    abandoned: Option<AbandonedTurnReport>,
}

impl FeedTerminal {
    /// The checkpoint it was read at.
    #[must_use]
    pub const fn checkpoint(&self) -> CheckpointId {
        self.checkpoint
    }

    /// The terminal result: completion, or the failure with its kind and
    /// diagnostic message.
    #[must_use]
    pub const fn result(&self) -> &TurnResult {
        &self.result
    }

    /// The abandonment report (claim, cause and closed operations) when the
    /// terminal closed an abandoned turn and its closure's parent was read.
    #[must_use]
    pub const fn abandoned_turn(&self) -> Option<&AbandonedTurnReport> {
        self.abandoned.as_ref()
    }

    /// Whether queued steering was settled unapplied.
    #[must_use]
    pub fn has_unapplied_steering(&self) -> bool {
        self.result.has_unapplied_steering()
    }
}

/// One terminal read.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum FeedTerminalOutcome {
    /// The terminal record.
    Terminal(Box<FeedTerminal>),
    /// The named checkpoint was pruned and the record is no longer retained.
    NotRetained,
}

/// The checkpoint a content read runs at.
struct FeedSource {
    checkpoint: Arc<RuntimeCheckpoint>,
    /// False when the named checkpoint was pruned and this is the head.
    exact: bool,
}

impl FeedSource {
    fn from_stored(
        stored: &StoredCheckpoint,
        session: SessionId,
        branch: BranchId,
        exact: bool,
    ) -> Result<Self, AgentSessionsError> {
        Self::from_checkpoint(Arc::clone(stored.checkpoint()), session, branch, exact)
    }

    fn from_checkpoint(
        checkpoint: Arc<RuntimeCheckpoint>,
        session: SessionId,
        branch: BranchId,
        exact: bool,
    ) -> Result<Self, AgentSessionsError> {
        let topology = checkpoint.manifest().topology();
        if topology.session_id != session || topology.branch_id != branch {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::InvalidInput,
                "the feed checkpoint belongs to another session or branch",
            ));
        }
        Ok(Self { checkpoint, exact })
    }

    fn id(&self) -> CheckpointId {
        self.checkpoint.manifest().topology().checkpoint_id
    }
}

fn not_held(what: &'static str) -> AgentSessionsError {
    AgentSessionsError::new(AgentSessionsErrorKind::InvalidInput, what)
}

fn read_items(
    source: &FeedSource,
    request: &FeedItemsRequest,
) -> Result<FeedItemsOutcome, AgentSessionsError> {
    if request.count == 0 || request.count > MAX_FEED_ITEMS {
        return Err(AgentSessionsError::new(
            AgentSessionsErrorKind::InvalidInput,
            "a feed items read names between 1 and 4096 items",
        ));
    }
    let manifest = source.checkpoint.manifest();
    let history = manifest.canonical().history();
    let all: Vec<&ConversationItem> = history
        .segments()
        .iter()
        .flat_map(plakous_model::HistorySegment::items)
        .collect();
    let count = usize::try_from(request.count).unwrap_or(usize::MAX);
    // A pruned checkpoint's fallback head locates the range by its first
    // entry; a chunk that names none is only ever read at its own checkpoint.
    let start = match (source.exact, request.first_entry) {
        (true, _) => usize::try_from(request.first_ordinal).ok(),
        (false, Some(first)) => all.iter().position(|item| item.id() == first),
        (false, None) => None,
    };
    let range = start.and_then(|start| Some(start..start.checked_add(count)?));
    let held = range.as_ref().and_then(|range| all.get(range.clone()));
    let Some(held) = held.filter(|held| {
        request
            .first_entry
            .is_none_or(|first| held.first().map(|item| item.id()) == Some(first))
            && request
                .last_entry
                .is_none_or(|last| held.last().map(|item| item.id()) == Some(last))
    }) else {
        if source.exact {
            return Err(not_held("the feed checkpoint does not hold these items"));
        }
        return Ok(FeedItemsOutcome::NotRetained);
    };
    let items: Vec<ConversationItem> = held.iter().map(|item| (*item).clone()).collect();
    Ok(FeedItemsOutcome::Items {
        checkpoint: source.id(),
        items: transcript::project_items(&items, manifest.ledger()),
    })
}

fn history_call(
    checkpoint: &RuntimeCheckpoint,
    call_id: CallId,
) -> Option<(ToolCallBlock, Option<ToolResultBlock>)> {
    let mut call = None;
    let mut result = None;
    let history = checkpoint.manifest().canonical().history();
    for block in history
        .segments()
        .iter()
        .flat_map(plakous_model::HistorySegment::items)
        .flat_map(ConversationItem::blocks)
    {
        match block {
            ContentBlock::ToolCall(block) if block.call_id() == call_id => {
                call = Some(block.clone());
            }
            ContentBlock::ToolResult(block) if block.call_id() == call_id => {
                result = Some(block.clone());
            }
            _ => {}
        }
    }
    call.map(|call| (call, result))
}

fn read_call(
    source: &FeedSource,
    target: FeedCallTarget,
) -> Result<FeedCallOutcome, AgentSessionsError> {
    let resolved = match target {
        FeedCallTarget::Call(call_id) => Some(FeedOperationCall::Call(call_id)),
        FeedCallTarget::Operation(operation_id) => {
            checkpoint_operation_call(&source.checkpoint, operation_id)
                .map_err(|error| runtime_error(&error))?
        }
    };
    let call = match resolved {
        Some(FeedOperationCall::Call(call_id)) => {
            history_call(&source.checkpoint, call_id).map(|(call, result)| FeedCall::Tool {
                call: Box::new(call),
                result: result.map(Box::new),
            })
        }
        Some(FeedOperationCall::Effect { call_id, requested }) => {
            Some(match history_call(&source.checkpoint, call_id) {
                Some((call, result)) => FeedCall::Tool {
                    call: Box::new(call),
                    result: result.map(Box::new),
                },
                None => FeedCall::Tool {
                    call: requested,
                    result: None,
                },
            })
        }
        Some(FeedOperationCall::Nested {
            call_id,
            name,
            arguments,
        }) => Some(FeedCall::Nested {
            call_id,
            name,
            arguments,
        }),
        Some(FeedOperationCall::Job(row)) => Some(FeedCall::Job {
            number: row.as_ref().map(|row| row.number),
            command: row.as_ref().map(|row| row.command.clone()),
            terminal: row
                .and_then(|row| row.terminal)
                .map(|terminal| FeedJobTerminal {
                    stopped: matches!(
                        terminal.status,
                        plakous_agent::internal::JobTerminalStatus::Stopped
                    ),
                    exit_code: terminal.exit_code,
                }),
        }),
        Some(FeedOperationCall::Steer(text)) => Some(FeedCall::Steer { text }),
        Some(_) | None => None,
    };
    match call {
        Some(call) => Ok(FeedCallOutcome::Call {
            checkpoint: source.id(),
            call,
        }),
        None if source.exact => Err(not_held("the feed checkpoint does not hold this call")),
        None => Ok(FeedCallOutcome::NotRetained),
    }
}

fn read_terminal(
    source: &FeedSource,
    turn_id: TurnId,
) -> Result<FeedTerminalOutcome, AgentSessionsError> {
    let terminal = if source.exact {
        checkpoint_turn_terminal(&source.checkpoint, turn_id)
    } else {
        checkpoint_retained_turn_terminal(&source.checkpoint, turn_id)
    }
    .map_err(|error| runtime_error(&error))?;
    match terminal {
        Some((result, abandoned)) => Ok(FeedTerminalOutcome::Terminal(Box::new(FeedTerminal {
            checkpoint: source.id(),
            result,
            abandoned,
        }))),
        None if source.exact => Err(not_held(
            "the feed checkpoint does not hold this turn's terminal",
        )),
        None => Ok(FeedTerminalOutcome::NotRetained),
    }
}

fn completed_terminal_source(
    source: &FeedSource,
    turn: TurnId,
) -> Result<Option<CheckpointId>, AgentSessionsError> {
    if source.exact {
        return Ok(None);
    }
    checkpoint_completed_terminal_source(&source.checkpoint, turn)
        .map(|checkpoint| checkpoint.filter(|id| *id != source.id()))
        .map_err(|error| runtime_error(&error))
}

impl AgentSessions {
    /// Reads the records after a cursor, oldest first, at most
    /// [`MAX_FEED_PAGE_RECORDS`] records or [`MAX_FEED_PAGE_BYTES`] encoded
    /// (one record always fits), with the tail observed in the same read.
    /// Needs no lease, takes no driver and writes nothing; available after
    /// archive and shutdown.
    ///
    /// # Errors
    ///
    /// Returns a typed lookup refusal for an unknown or deleted session, an
    /// invalid-input refusal above the page bound, or a backend failure.
    pub async fn feed_page(
        &self,
        request: FeedPageRequest,
    ) -> Result<FeedPage, AgentSessionsError> {
        let request = request.validate()?;
        let session = request.cursor.session_id();
        self.store
            .read_feed(session, request.cursor.applied(), request.max_records)
            .await
            .map(FeedPage::from_store)
            .map_err(|error| store_error(error).for_session(session))
    }

    /// Subscribes to one session's feed tail. The subscription starts at the
    /// tail observed now and resolves after the store acknowledges each later
    /// mutation of that session.
    ///
    /// # Errors
    ///
    /// Returns a typed lookup refusal for an unknown or deleted session, or a
    /// backend failure.
    pub async fn subscribe_feed(
        &self,
        session_id: SessionId,
    ) -> Result<FeedSubscription, AgentSessionsError> {
        // Subscribe before reading the tail so no acknowledged mutation can
        // fall between the two.
        let changes = self.feed.subscribe(session_id);
        let seen = self
            .store
            .feed_tail(session_id)
            .await
            .map_err(|error| store_error(error).for_session(session_id))?
            .tail();
        Ok(FeedSubscription {
            session_id,
            store: Arc::clone(&self.store),
            changes,
            seen,
        })
    }

    /// Reads the items a `HistoryAppended` fact names, as the transcript's
    /// presentation parts, at the record's checkpoint. Needs no lease, takes
    /// no driver and writes nothing; a checkpoint other than the head is never
    /// a stale refusal. When the checkpoint was pruned the read falls back to
    /// the branch head by entry identity, and items a compaction removed since
    /// are [`FeedItemsOutcome::NotRetained`].
    ///
    /// # Errors
    ///
    /// Returns typed lookup, invalid-input (a checkpoint of another branch, or
    /// one that does not hold the items), integrity or backend failure.
    pub async fn feed_items(
        &self,
        request: FeedItemsRequest,
    ) -> Result<FeedItemsOutcome, AgentSessionsError> {
        let source = self
            .feed_source(request.session, request.branch, request.checkpoint)
            .await?;
        read_items(&source, &request)
    }

    /// Reads one call at a record's checkpoint: an effect's or Code Cell's
    /// committed call block and result, a process session's job, or a steer's
    /// text, by operation id or, for a call rejected before preparation, by
    /// call id. The result is returned whole as committed. Pruned checkpoints
    /// fall back to the branch head as [`Self::feed_items`] does.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, invalid-input, integrity or backend failure.
    pub async fn feed_call(
        &self,
        request: FeedCallRequest,
    ) -> Result<FeedCallOutcome, AgentSessionsError> {
        let source = self
            .feed_source(request.session, request.branch, request.checkpoint)
            .await?;
        read_call(&source, request.call)
    }

    /// Reads a root turn's committed terminal record at a record's
    /// checkpoint. Pruned checkpoints fall back to the branch head as
    /// [`Self::feed_items`] does.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, invalid-input, integrity or backend failure.
    pub async fn feed_terminal(
        &self,
        request: FeedTerminalRequest,
    ) -> Result<FeedTerminalOutcome, AgentSessionsError> {
        let source = self
            .feed_source(request.session, request.branch, request.checkpoint)
            .await?;
        if let Some(checkpoint) = completed_terminal_source(&source, request.turn_id)? {
            let loaded = match self.store.load_checkpoint(checkpoint).await {
                Ok(stored) => Arc::clone(stored.checkpoint()),
                Err(SessionStoreError::Pruned) => return Ok(FeedTerminalOutcome::NotRetained),
                Err(error) => return Err(store_error(error).for_session(request.session)),
            };
            return read_terminal(
                &FeedSource::from_checkpoint(loaded, request.session, request.branch, true)?,
                request.turn_id,
            );
        }
        read_terminal(&source, request.turn_id)
    }

    /// Reads terminal evidence in request order, sharing checkpoint and pruned
    /// branch-head loads within this batch. Each result retains the singleton
    /// read's typed lookup, invalid-input, integrity or backend refusal.
    pub async fn feed_terminals(
        &self,
        requests: Vec<FeedTerminalRequest>,
    ) -> Vec<Result<FeedTerminalOutcome, AgentSessionsError>> {
        use std::collections::BTreeMap;
        let mut checkpoints = BTreeMap::new();
        let mut heads = BTreeMap::new();
        let mut outcomes = Vec::with_capacity(requests.len());
        for request in requests {
            let checkpoint = match checkpoints.entry(request.checkpoint) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let loaded = if let Some(published) = self
                        .feed
                        .published_checkpoint(request.session, request.checkpoint)
                    {
                        Ok(Some(published))
                    } else {
                        match self.store.load_checkpoint(request.checkpoint).await {
                            Ok(stored) => Ok(Some(Arc::clone(stored.checkpoint()))),
                            Err(SessionStoreError::Pruned) => Ok(None),
                            Err(error) => Err(store_error(error)),
                        }
                    };
                    entry.insert(loaded)
                }
            };
            let source = match checkpoint {
                Ok(Some(checkpoint)) => FeedSource::from_checkpoint(
                    Arc::clone(checkpoint),
                    request.session,
                    request.branch,
                    true,
                ),
                Ok(None) => {
                    let head = match heads.entry((request.session, request.branch)) {
                        std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::btree_map::Entry::Vacant(entry) => entry.insert(
                            self.store
                                .load_branch(plakous_agent::internal::SessionRef::new(
                                    request.session,
                                    request.branch,
                                ))
                                .await
                                .map(|head| Arc::clone(head.checkpoint().checkpoint()))
                                .map_err(store_error),
                        ),
                    };
                    match head {
                        Ok(head) => FeedSource::from_checkpoint(
                            Arc::clone(head),
                            request.session,
                            request.branch,
                            false,
                        ),
                        Err(error) => Err(error.clone()),
                    }
                }
                Err(error) => Err(error.clone()),
            };
            let result = match source {
                Ok(source) => match completed_terminal_source(&source, request.turn_id) {
                    Ok(Some(id)) => {
                        let loaded = match checkpoints.entry(id) {
                            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                            std::collections::btree_map::Entry::Vacant(entry) => {
                                entry.insert(match self.store.load_checkpoint(id).await {
                                    Ok(stored) => Ok(Some(Arc::clone(stored.checkpoint()))),
                                    Err(SessionStoreError::Pruned) => Ok(None),
                                    Err(error) => Err(store_error(error)),
                                })
                            }
                        };
                        match loaded {
                            Ok(Some(checkpoint)) => FeedSource::from_checkpoint(
                                Arc::clone(checkpoint),
                                request.session,
                                request.branch,
                                true,
                            )
                            .and_then(|terminal| read_terminal(&terminal, request.turn_id)),
                            Ok(None) => Ok(FeedTerminalOutcome::NotRetained),
                            Err(error) => Err(error.clone()),
                        }
                    }
                    Ok(None) => read_terminal(&source, request.turn_id),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            outcomes.push(result.map_err(|error| error.for_session(request.session)));
        }
        outcomes
    }

    async fn feed_source(
        &self,
        session: SessionId,
        branch: BranchId,
        checkpoint: CheckpointId,
    ) -> Result<FeedSource, AgentSessionsError> {
        // The live runtime's newest commits are served from memory; any other
        // checkpoint is the store's (spec 01 §5.11).
        if let Some(published) = self.feed.published_checkpoint(session, checkpoint) {
            return FeedSource::from_checkpoint(published, session, branch, true);
        }
        match self.store.load_checkpoint(checkpoint).await {
            Ok(stored) => FeedSource::from_stored(&stored, session, branch, true),
            Err(SessionStoreError::Pruned) => {
                let head = self
                    .store
                    .load_branch(plakous_agent::internal::SessionRef::new(session, branch))
                    .await
                    .map_err(|error| store_error(error).for_session(session))?;
                FeedSource::from_stored(head.checkpoint(), session, branch, false)
            }
            Err(error) => Err(store_error(error).for_session(session)),
        }
    }

    /// Read-only SQLite twin of [`Self::feed_page`], over one snapshot, with
    /// no runtime, lease or write.
    ///
    /// # Errors
    ///
    /// Returns the same typed refusals as [`Self::feed_page`].
    #[cfg(feature = "sessions-sqlite")]
    pub fn inspect_feed_sqlite(
        path: impl AsRef<Path>,
        request: FeedPageRequest,
    ) -> Result<FeedPage, AgentSessionsError> {
        let request = request.validate()?;
        let session = request.cursor.session_id();
        inspect_sqlite_feed(
            path.as_ref(),
            session,
            request.cursor.applied(),
            request.max_records,
        )
        .map(FeedPage::from_store)
        .map_err(|error| store_error(error).for_session(session))
    }

    /// Read-only SQLite twin of [`Self::feed_items`], over one snapshot.
    ///
    /// # Errors
    ///
    /// Returns the same typed refusals as [`Self::feed_items`].
    #[cfg(feature = "sessions-sqlite")]
    pub fn inspect_feed_items_sqlite(
        path: impl AsRef<Path>,
        request: &FeedItemsRequest,
    ) -> Result<FeedItemsOutcome, AgentSessionsError> {
        let source = inspect_feed_source(
            path.as_ref(),
            request.session,
            request.branch,
            request.checkpoint,
        )?;
        read_items(&source, request)
    }

    /// Read-only SQLite twin of [`Self::feed_call`], over one snapshot.
    ///
    /// # Errors
    ///
    /// Returns the same typed refusals as [`Self::feed_call`].
    #[cfg(feature = "sessions-sqlite")]
    pub fn inspect_feed_call_sqlite(
        path: impl AsRef<Path>,
        request: &FeedCallRequest,
    ) -> Result<FeedCallOutcome, AgentSessionsError> {
        let source = inspect_feed_source(
            path.as_ref(),
            request.session,
            request.branch,
            request.checkpoint,
        )?;
        read_call(&source, request.call)
    }

    /// Read-only SQLite twin of [`Self::feed_terminal`]. A pruned request may
    /// read its immutable selected terminal checkpoint in a second snapshot.
    /// Its abandonment report is read from the terminal record itself.
    ///
    /// # Errors
    ///
    /// Returns the same typed refusals as [`Self::feed_terminal`].
    #[cfg(feature = "sessions-sqlite")]
    pub fn inspect_feed_terminal_sqlite(
        path: impl AsRef<Path>,
        request: &FeedTerminalRequest,
    ) -> Result<FeedTerminalOutcome, AgentSessionsError> {
        let source = inspect_feed_source(
            path.as_ref(),
            request.session,
            request.branch,
            request.checkpoint,
        )?;
        if let Some(checkpoint) = completed_terminal_source(&source, request.turn_id)? {
            let stored = match SqliteSessionStore::inspect_checkpoint(path.as_ref(), checkpoint) {
                Ok(stored) => stored,
                Err(SessionStoreError::Pruned) => return Ok(FeedTerminalOutcome::NotRetained),
                Err(error) => return Err(store_error(error).for_session(request.session)),
            };
            let terminal = FeedSource::from_stored(&stored, request.session, request.branch, true)?;
            return read_terminal(&terminal, request.turn_id);
        }
        read_terminal(&source, request.turn_id)
    }
}

/// Reads one feed page from a read-only snapshot of the SQLite store.
#[cfg(feature = "sessions-sqlite")]
fn inspect_sqlite_feed(
    path: &Path,
    session: SessionId,
    after: FeedSeq,
    max_records: NonZeroUsize,
) -> Result<StoreFeedPage, SessionStoreError> {
    SqliteSessionStore::inspect_feed(path, session, after, max_records)
}

#[cfg(feature = "sessions-sqlite")]
fn inspect_feed_source(
    path: &Path,
    session: SessionId,
    branch: BranchId,
    checkpoint: CheckpointId,
) -> Result<FeedSource, AgentSessionsError> {
    // The named checkpoint is loaded directly, exactly as the live
    // `feed_source` does: one read per call, however long the branch.
    match SqliteSessionStore::inspect_checkpoint(path, checkpoint) {
        Ok(stored) => FeedSource::from_stored(&stored, session, branch, true),
        Err(SessionStoreError::Pruned) => {
            let selector = plakous_agent::internal::SessionRef::new(session, branch);
            let head = SqliteSessionStore::inspect_branch(path, selector)
                .map_err(|error| store_error(error).for_session(session))?;
            FeedSource::from_stored(head.checkpoint(), session, branch, false)
        }
        Err(error) => Err(store_error(error).for_session(session)),
    }
}
