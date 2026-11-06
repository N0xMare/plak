//! Canonical checkpoint publication and operation transitions.

mod checkpoint;
mod commit;
mod effects;
mod promotion;
mod terminal;
mod transitions;

use crate::manifest::RuntimeContextState;
use crate::runtime::checkpoint::candidate::SuccessorFacts;
use crate::runtime::checkpoint::identity::history_segment_kind;
use crate::runtime::checkpoint::io::DurabilityCommitTelemetry;
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::control::{
    durable_command_effect, durable_control_error_kind, durable_rejection, effect_intent_bytes,
};
use crate::runtime::driver::Driver;
use crate::runtime::driver::admitted_round::AdmittedRound;
use crate::runtime::driver::notices;
use crate::runtime::observation::{
    retained_tool_exchange_failure, shrink_tool_exchange_observations, tool_exchange_shrink_budget,
};
use crate::runtime::operation_record::{
    FailTurnShape, ProviderOutcome, RoundDecision, effect_operation_intent_digest,
    ledger_needs_retention, operation_kind_turn_id, operation_turn_id, provider_outcome,
    provider_terminal_state, retain_quiescent_ledger, working_lifecycle,
    working_lifecycle_for_root,
};
use crate::runtime::terminal::{
    DurableAgentError, DurableTerminalOutcome, DurableTerminalResultRecord,
    PendingTerminalResultRecord, TERMINAL_RESULT_RECORD_VERSION, cancelled_error,
    encode_pending_terminal_record, encode_terminal_result_record, finalized_operation_evidence,
};
use crate::runtime::{
    ArtifactTransition, DurableActorCommandEffect, DurableCommandEnvelope, OperationId,
    PromptCommand,
};
use crate::tool_runtime::{AuthorizedToolInvocation, ClaimedToolInvocation, EffectDispatchIntent};
use crate::{
    ActorCommandEffect, AgentError, AgentErrorKind, AgentEventKind, CheckpointId, CommandId,
    ControlError, DurableCommandDisposition, DurableCommandRecord, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableOperationTransition,
    EffectDecisionResolution, EventContext, FamilyArtifact, RuntimeCheckpoint,
    RuntimeComponentKind, RuntimeLifecycle, RuntimeOperationLedger, RuntimeWorkingRoot,
    SessionStoreError, StoreAdmission, TurnUsage,
};
use plakous_model::{
    ActiveFamilyArtifact, Actor, AttemptId, CanonicalHistory, ContentBlock, ContextStatusKind,
    ConversationItem, ConversationView, Digest, EntryId, HistorySegment, HistorySegmentKind,
    ModelCacheAffinity, ModelError, ModelErrorKind, ModelErrorPhase, ModelOperationRoot,
    ModelRequest, ModelResponse, PreparedCallRecord, Provenance, RequestMetadata, TurnId,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// How a turn ends in its consequence commit.
pub(super) enum TurnConsequence<'a> {
    /// The final response, committed with its artifact transition.
    Completed {
        response: &'a ModelResponse,
        artifact: ArtifactTransition,
    },
    /// A cancellation or failure; an `OutcomeUnknown` error records
    /// `RecoveryRequired`.
    Ended(&'a AgentError),
}

/// One atomic successor under construction: every decision commits with its
/// consequences in exactly one checkpoint CAS (spec 01 §3.5).
pub(super) struct CommitPlan {
    pub(super) checkpoint_id: CheckpointId,
    /// Ledger transitions applied in order before anything is finalized.
    pub(super) transitions: Vec<(OperationId, DurableOperationState)>,
    /// Records appended to the ledger after the transitions.
    pub(super) appended: Vec<DurableOperationRecord>,
    /// Operations finalized against the committed history.
    pub(super) finalize: BTreeSet<OperationId>,
    /// Also finalize every same-turn record still owing finalization; any
    /// unresolved same-turn record refuses the commit.
    pub(super) finalize_turn: bool,
    /// Drops the trailing segment, which the caller has proven is this
    /// root's own unanswered prompt, before appending.
    pub(super) withhold_prompt: bool,
    pub(super) segments: Vec<HistorySegment>,
    pub(super) artifact: ArtifactTransition,
    /// Process notices the appended history consumes.
    pub(super) notices: Vec<u32>,
}

/// Every record of `turn_id` that is settled but not yet finalized. A
/// record whose outcome is not settled cannot be closed by a turn's
/// consequence commit.
fn settled_turn_operations(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
    closing: &BTreeSet<OperationId>,
) -> Result<Vec<OperationId>, AgentError> {
    let mut operations = Vec::new();
    for record in ledger.operations() {
        if operation_turn_id(record) != Some(turn_id) {
            continue;
        }
        match record.state() {
            DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_) => {
                operations.push(OperationId(record.operation_id()));
            }
            // A cancel of a restored hosted frame names that baton in
            // `closing` and finalizes it with the evidence it already
            // carries. A live continuation is not named, so it still refuses
            // the commit and the turn recovers. Cancellation is not a
            // definite refusal, so the fence does not close either first.
            DurableOperationState::ContinuationRequired(_)
                if matches!(
                    record.kind(),
                    DurableOperationKind::ToolBatchContinuation { .. }
                        | DurableOperationKind::ServerPauseContinuation { .. }
                ) && closing.contains(&OperationId(record.operation_id())) => {}
            DurableOperationState::Prepared
            | DurableOperationState::DecisionRequired(_)
            | DurableOperationState::ContinuationRequired(_)
            | DurableOperationState::Claimed(_)
            | DurableOperationState::Dispatched
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::OutcomeUnknown(_) => {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "terminal commit has an unresolved durable operation",
                ));
            }
            DurableOperationState::Finalized(_) => {}
        }
    }
    Ok(operations)
}

/// Whether the terminal commit of a turn that ended with `error` keeps the
/// family artifact. A context-capacity failure never started a new provider
/// round, so the artifact stays; any other failure clears it. Shared by the
/// live terminal commit and recovery's finish row.
pub(in crate::runtime) fn failure_preserves_artifact(error: &AgentError) -> bool {
    error
        .model_error()
        .and_then(ModelError::context_capacity)
        .is_some()
}

/// What the terminal commit of a cancelled or failed root adds after its
/// prompt.
pub(in crate::runtime) enum TerminalPromptTail {
    Unchanged,
    Note(ConversationItem),
    /// Replaces the trailing prompt with the note.
    WithholdPrompt(ConversationItem),
}

/// The tail the terminal commit of a cancelled or failed turn appends after
/// its prompt, shared by the live terminal commit and recovery's finish row
/// (spec 01 §5.5) so both publish the same consequence. `withhold` is the
/// measurement a `FailTurn` decision recorded; `trailing` the turn's prompt
/// when it is still the last history group.
pub(in crate::runtime) fn failure_prompt_tail(
    turn_id: TurnId,
    error: &AgentError,
    withhold: bool,
    trailing: Option<(usize, Provenance)>,
    abandonment_note: Option<&str>,
) -> Result<TerminalPromptTail, AgentError> {
    match error.kind() {
        AgentErrorKind::OutcomeUnknown => return Ok(TerminalPromptTail::Unchanged),
        AgentErrorKind::Cancelled => {
            let note = match abandonment_note {
                Some(note) => turn_note(
                    turn_id,
                    b"plakous-agent/abandoned-turn-entry-v1",
                    note.to_owned(),
                )?,
                None => cancelled_prompt_note(turn_id)?,
            };
            return Ok(TerminalPromptTail::Note(note));
        }
        _ => {}
    }
    if withhold && let Some((bytes, provenance)) = trailing {
        return Ok(TerminalPromptTail::WithholdPrompt(withheld_prompt_note(
            turn_id,
            bytes,
            provenance == Provenance::UserInput,
        )?));
    }
    Ok(TerminalPromptTail::Note(failed_prompt_note(
        turn_id, error,
    )?))
}

/// The turn's prompt, when it is exactly the last history group: the item
/// `prompt` names, or for a recovered head that no longer knows it, the one
/// user item of a trailing recent-intent group.
pub(in crate::runtime) fn trailing_prompt(
    history: &CanonicalHistory,
    prompt: Option<EntryId>,
) -> Option<(usize, Provenance)> {
    let last = history.segments().iter().last()?;
    match last.items() {
        [item]
            if prompt.map_or(
                last.kind() == HistorySegmentKind::RecentIntent && item.actor() == Actor::User,
                |prompt| item.id() == prompt,
            ) =>
        {
            Some((
                item.blocks()
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text(text) => text.text().len(),
                        _ => 0,
                    })
                    .sum(),
                item.provenance(),
            ))
        }
        _ => None,
    }
}

impl TerminalPromptTail {
    /// Whether the terminal commit drops the trailing prompt, and the segment
    /// it appends.
    ///
    /// A note after an unanswered prompt is conversation: it opens no root
    /// turn. A withheld prompt's note takes the prompt's place as the turn's
    /// recent intent, so the turn it stands for still counts (spec 01 §10.6).
    pub(in crate::runtime) fn into_commit(
        self,
    ) -> Result<(bool, Option<HistorySegment>), AgentError> {
        let (withhold_prompt, kind, note) = match self {
            Self::Unchanged => return Ok((false, None)),
            Self::Note(note) => (false, HistorySegmentKind::Conversation, note),
            Self::WithholdPrompt(note) => (true, HistorySegmentKind::RecentIntent, note),
        };
        let segment = HistorySegment::new(kind, vec![note]).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "terminal prompt note is not a valid history group",
            )
        })?;
        Ok((withhold_prompt, Some(segment)))
    }
}

/// The harness note a cancelled turn leaves after its unanswered prompt.
///
/// The prompt stays in canonical history, which only appends. Without this
/// the next turn's model reads it as still pending and answers it alongside
/// the new request.
/// The prepared call and attempt a provider dispatch rejection must match.
fn expected_provider_dispatch(record: &DurableOperationRecord) -> Option<(Digest, AttemptId)> {
    match record.kind() {
        DurableOperationKind::Provider { attempt_id, .. } => {
            serde_json::from_slice::<PreparedCallRecord>(record.request().bytes())
                .ok()
                .map(|prepared| (prepared.identity(), *attempt_id))
        }
        _ => None,
    }
}

fn cancelled_prompt_note(turn_id: TurnId) -> Result<ConversationItem, AgentError> {
    turn_note(
        turn_id,
        b"plakous-agent/cancelled-prompt-note-v1",
        "the user cancelled the request above before it was answered. Do not \
         continue or act on it unless the user asks for it again."
            .to_owned(),
    )
}

/// The harness note a failed turn leaves after its unanswered prompt. It names
/// the error class only: the diagnostic may quote provider or tool text.
fn failed_prompt_note(turn_id: TurnId, error: &AgentError) -> Result<ConversationItem, AgentError> {
    let class = error
        .model_kind()
        .map_or_else(|| format!("{:?}", error.kind()), |kind| format!("{kind:?}"));
    turn_note(
        turn_id,
        b"plakous-agent/failed-prompt-note-v1",
        format!(
            "the turn handling the request above failed ({class}) before it was answered. \
             Tool results above are what actually ran. Do not continue or act on the request \
             unless the user asks for it again."
        ),
    )
}

/// The note that stands where a prompt too large for the model's window was.
/// `person` is whether a person submitted that prompt (not a host wake).
fn withheld_prompt_note(
    turn_id: TurnId,
    bytes: usize,
    person: bool,
) -> Result<ConversationItem, AgentError> {
    let entry = withheld_prompt_entry_id(turn_id, person).ok_or_else(turn_note_failed)?;
    turn_note_with_entry(
        entry,
        format!(
            "a user message of {bytes} bytes was withheld here: it did not fit the model's \
             context window, so it was never answered. Do not guess at its content; the user \
             can send it again in a smaller form."
        ),
    )
}

const WITHHELD_PROMPT_NOTE_DOMAIN: &[u8] = b"plakous-agent/withheld-prompt-note-v2";
const WITHHELD_WAKE_NOTE_DOMAIN: &[u8] = b"plakous-agent/withheld-wake-note-v1";
/// The leading bytes of a withheld person's prompt's note identity. The rest
/// is derived from the turn, as for every turn note.
const WITHHELD_PROMPT_ENTRY_TAG: [u8; 6] = *b"plkwhp";

/// The entry identity of the note that replaced `turn_id`'s withheld prompt;
/// `person` as for `withheld_prompt_note`.
///
/// A person's prompt's note is recognisable from its identity alone
/// (`is_withheld_prompt_entry`), so the turn it stands for still counts as a
/// person's without reading its text.
pub(in crate::runtime) fn withheld_prompt_entry_id(
    turn_id: TurnId,
    person: bool,
) -> Option<EntryId> {
    if !person {
        return turn_note_entry_id(turn_id, WITHHELD_WAKE_NOTE_DOMAIN);
    }
    let mut bytes = turn_note_bytes(turn_id, WITHHELD_PROMPT_NOTE_DOMAIN)?;
    bytes[..WITHHELD_PROMPT_ENTRY_TAG.len()].copy_from_slice(&WITHHELD_PROMPT_ENTRY_TAG);
    EntryId::try_from(uuid::Uuid::from_bytes(bytes)).ok()
}

/// Whether `entry` is the note that replaced a person's withheld prompt.
pub(in crate::runtime) fn is_withheld_prompt_entry(entry: EntryId) -> bool {
    let uuid = entry.as_uuid();
    uuid.get_version_num() == 8 && uuid.as_bytes().starts_with(&WITHHELD_PROMPT_ENTRY_TAG)
}

pub(super) fn turn_note_entry_id(turn_id: TurnId, domain: &[u8]) -> Option<EntryId> {
    EntryId::try_from(uuid::Uuid::from_bytes(turn_note_bytes(turn_id, domain)?)).ok()
}

/// A version-8 identity derived from `turn_id` and `domain`.
fn turn_note_bytes(turn_id: TurnId, domain: &[u8]) -> Option<[u8; 16]> {
    let digest = Digest::sha256(
        plakous_model::DigestDomain::ProviderOperation,
        2,
        &[domain, turn_id.as_uuid().as_bytes()],
    )
    .ok()?;
    let bytes = crate::derived_id::bytes(digest);
    Some(bytes)
}

/// One harness note per turn and `domain`. `User` with
/// `Provenance::Instruction` is the harness spelling every wire can carry (see
/// `abandoned_turn_segment`), and the entry id is derived from the turn so a
/// retried terminal commit adds it once.
fn turn_note(turn_id: TurnId, domain: &[u8], text: String) -> Result<ConversationItem, AgentError> {
    let entry = turn_note_entry_id(turn_id, domain).ok_or_else(turn_note_failed)?;
    turn_note_with_entry(entry, text)
}

fn turn_note_with_entry(entry: EntryId, text: String) -> Result<ConversationItem, AgentError> {
    let text = plakous_model::TextBlock::new(text, None).map_err(|_| turn_note_failed())?;
    ConversationItem::new(
        entry,
        Actor::User,
        vec![ContentBlock::Text(text)],
        Provenance::Instruction,
    )
    .map_err(|_| turn_note_failed())
}

fn turn_note_failed() -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        "terminal turn note could not be built",
    )
}

/// Store admission of one operation transition. Only the first fence out of
/// `Prepared` (a claim, a prepared locator, or the dispatch itself) starts new
/// work: a refusal there leaves the operation `Prepared`, which the turn's
/// settlement commit records as proven not dispatched. Every later step of an
/// operation that already crossed a fence records work that has begun.
pub(super) fn transition_admission(
    current: &DurableOperationState,
    target: &DurableOperationState,
) -> StoreAdmission {
    match (current, target) {
        (
            DurableOperationState::Prepared,
            DurableOperationState::Claimed(_)
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::Dispatched,
        ) => StoreAdmission::Ordinary,
        _ => StoreAdmission::Settlement,
    }
}

#[cfg(test)]
mod terminal_note_turn_position_tests;
