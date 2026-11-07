//! Content-free, read-only ledger facts and scripted provider responses for the real-binary crash sweep.
use plakous::sessions::SessionLocator;
use plakous_agent::internal::{
    DurableOperationKind as Kind, DurableOperationState as State, SessionRef,
};
use plakous_store::SqliteSessionStore;
use serde::Serialize;
use std::path::Path;

#[path = "root_terminal_answers.rs"]
mod root_terminal_answers;
pub use root_terminal_answers::{inspect_completed_root_answers, inspect_root_lifecycles};

/// The committed compaction strategy and number of protected trailing segments.
///
/// # Errors
/// Returns read-only store lookup and integrity failures.
pub fn inspect_compaction_strategy(
    database: &Path,
    head: SessionLocator,
) -> Result<Option<(String, u32)>, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    Ok(loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .map(|provenance| {
            (
                format!("{:?}", provenance.strategy()),
                provenance.trailing_segment_count(),
            )
        }))
}

/// Reads only `UserInput` steering text from the canonical history of a fixture.
///
/// # Errors
/// Returns read-only store lookup and integrity failures.
pub fn inspect_steering_text(
    database: &Path,
    head: SessionLocator,
) -> Result<Vec<String>, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    Ok(loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .filter(|segment| segment.kind() == plakous_model::HistorySegmentKind::Steering)
        .flat_map(plakous_model::HistorySegment::items)
        .filter(|item| {
            item.actor() == plakous_model::Actor::User
                && item.provenance() == plakous_model::Provenance::UserInput
        })
        .flat_map(plakous_model::ConversationItem::blocks)
        .filter_map(|block| match block {
            plakous_model::ContentBlock::Text(text) => Some(text.text().to_owned()),
            _ => None,
        })
        .collect())
}

/// Reads fixture process-notice identities from canonical instruction history.
/// Host job snapshots are observational and are not notice receipts.
///
/// # Errors
/// Returns the store's typed schema, integrity or lookup error.
pub fn inspect_process_notices(
    database: &Path,
    head: SessionLocator,
) -> Result<Vec<u32>, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    Ok(loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(plakous_model::HistorySegment::items)
        .filter(|item| {
            item.actor() == plakous::model::Actor::User
                && item.provenance() == plakous::model::Provenance::Instruction
        })
        .flat_map(plakous::model::ConversationItem::blocks)
        .filter_map(|block| match block {
            plakous::model::ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .flat_map(|text| text.split("<process_notice id=\"p").skip(1))
        .filter_map(|text| text.split_once('"')?.0.parse().ok())
        .collect())
}

/// Content-free durable process rows, without commands, output or process ids.
#[derive(Debug, Serialize)]
pub struct JobFact {
    /// Session-local job number.
    pub number: u32,
    /// Exact terminal evidence is present, rather than an unknown live process.
    pub terminal: bool,
}

/// Reads durable unconsumed job notices without opening a writable store.
///
/// # Errors
/// Returns the store's typed schema, integrity or lookup error.
pub fn inspect_jobs(
    database: &Path,
    head: SessionLocator,
) -> Result<Vec<JobFact>, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    Ok(loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .jobs()
        .rows()
        .iter()
        .map(|job| JobFact {
            number: job.number,
            terminal: job.terminal.is_some(),
        })
        .collect())
}

/// Reads the exact promoted family artifact without opening a writable store.
///
/// # Errors
/// Returns the store's typed schema, integrity or lookup error.
pub fn inspect_family_artifact(
    database: &Path,
    head: SessionLocator,
) -> Result<
    Option<plakous_agent::internal::FamilyArtifact>,
    plakous_agent::internal::SessionStoreError,
> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    Ok(loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .family_artifact()
        .cloned())
}

/// One decoded operation's identity, class and latest durable state.
#[derive(Debug, PartialEq, Serialize)]
pub struct LedgerFact {
    /// Stable operation ordinal within the ledger.
    pub operation: u64,
    /// Closed operation kind, without request contents or private identities.
    pub kind: &'static str,
    /// Closed latest transition, without evidence contents.
    pub state: &'static str,
    /// The fixture answer marker occurs in this settled operation's evidence.
    pub answer_marker: bool,
}

/// Reads retirement twins' operation facts and exact tagged counter bytes.
/// Only the bound root UUID is alpha-renamed after checking it names a retained
/// root command: independently created twins mint different root identities.
///
/// # Errors
/// Returns typed store/schema errors or a malformed counter encoding.
pub fn inspect_retirement_twin(
    database: &Path,
    head: SessionLocator,
) -> Result<(Vec<LedgerFact>, Vec<u8>), plakous_agent::internal::SessionStoreError> {
    use plakous_agent::internal::{RuntimeCheckpointCodec, RuntimeObjectKind, SessionStoreError};
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    let checkpoint = loaded.checkpoint().checkpoint();
    let bundle = RuntimeCheckpointCodec::bundle(checkpoint, None)
        .map_err(|_| SessionStoreError::corrupt())?;
    let object = bundle
        .objects()
        .iter()
        .find(|object| object.address().kind() == RuntimeObjectKind::OperationLedger)
        .ok_or_else(SessionStoreError::corrupt)?;
    let bytes = object.canonical_bytes();
    let mut offset = 0;
    for _ in 0..2 {
        offset += 8;
        let flag = *bytes.get(offset).ok_or_else(SessionStoreError::corrupt)?;
        offset += 1 + if flag == 1 { 37 } else { 0 };
    }
    let length = u32::from_be_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(SessionStoreError::corrupt)?
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
    ) as usize;
    let mut counter = bytes
        .get(offset + 4..)
        .ok_or_else(SessionStoreError::corrupt)?
        .to_vec();
    if counter.len() != length {
        return Err(SessionStoreError::corrupt());
    }
    match counter.as_mut_slice() {
        [0] => {}
        [1, rest @ ..] if rest.len() == 24 => {
            let root =
                uuid::Uuid::from_slice(&rest[..16]).map_err(|_| SessionStoreError::corrupt())?;
            if !checkpoint.manifest().ledger().operations().iter().any(|record| matches!(record.kind(), Kind::Command { turn_id: Some(turn) } if turn.as_uuid() == root)) { return Err(SessionStoreError::corrupt()); }
            rest[..16].fill(0);
        }
        _ => return Err(SessionStoreError::corrupt()),
    }
    Ok((ledger_facts(checkpoint.manifest().ledger(), ""), counter))
}

/// Retirement witnesses read from one immutable checkpoint, even with a live owner.
pub struct LiveFrontierFacts {
    /// Content-free operations at that checkpoint.
    pub ledger: Vec<LedgerFact>,
    /// Whether the checkpoint has a working root.
    pub working: bool,
    /// Whether at least one promoted process has no terminal yet.
    pub live_process: bool,
}

/// Reads the live branch once without racing separate exact-head inspections.
///
/// # Errors
/// Returns the store's typed schema, integrity or lookup error.
pub fn inspect_live_frontier(
    database: &Path,
    head: SessionLocator,
) -> Result<LiveFrontierFacts, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    let manifest = loaded.checkpoint().checkpoint().manifest();
    Ok(LiveFrontierFacts {
        ledger: ledger_facts(manifest.ledger(), ""),
        working: !matches!(
            manifest.lifecycle(),
            plakous_agent::internal::RuntimeLifecycle::Quiescent
        ),
        live_process: manifest
            .jobs()
            .rows()
            .iter()
            .any(|job| job.terminal.is_none()),
    })
}

/// Decodes the authoritative head through agent internals without writable open.
///
/// # Errors
/// Returns the store's typed schema, integrity or lookup error.
pub fn inspect_ledger(
    database: &Path,
    head: SessionLocator,
    answer: &str,
) -> Result<Vec<LedgerFact>, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    Ok(ledger_facts(
        loaded.checkpoint().checkpoint().manifest().ledger(),
        answer,
    ))
}

fn ledger_facts(
    ledger: &plakous_agent::internal::RuntimeOperationLedger,
    answer: &str,
) -> Vec<LedgerFact> {
    ledger
        .operations()
        .iter()
        .map(|op| {
            let kind = match op.kind() {
                Kind::Command { .. } => "command",
                Kind::Provider { .. } => "provider",
                Kind::EffectDecision { .. } => "effect_decision",
                Kind::Effect { .. } => "effect",
                Kind::ToolBatchContinuation { .. } => "tool_batch",
                Kind::ServerPauseContinuation { .. } => "server_pause",
                Kind::CompositionEvaluation { .. } => "composition_evaluation",
                Kind::CompositionContinuation { .. } => "composition_continuation",
                Kind::CompositionChild { .. } => "composition_child",
                Kind::ProcessSession { .. } => "process_session",
                Kind::Commit { .. } => "commit",
                Kind::Compaction { .. } => "compaction",
            };
            let state = match op.state() {
                State::Prepared => "prepared",
                State::DecisionRequired(_) => "decision_required",
                State::ContinuationRequired(_) => "continuation_required",
                State::Claimed(_) => "claimed",
                State::Dispatched => "dispatched",
                State::LocatorBound(_) => "locator_bound",
                State::Completed(_) => "completed",
                State::ProvenNotDispatched(_) => "proven_not_dispatched",
                State::OutcomeUnknown(_) => "outcome_unknown",
                State::Finalized(_) => "finalized",
            };
            let answer_marker = match op.state() {
                State::Completed(evidence) | State::Finalized(evidence) => {
                    root_answer_marker(op.kind(), evidence.bytes(), answer)
                }
                _ => false,
            };
            LedgerFact {
                operation: op.operation_id(),
                kind,
                state,
                answer_marker,
            }
        })
        .collect()
}

/// Content-free ownership evidence retained alongside a crash finding.
#[derive(Debug, PartialEq, Serialize)]
pub struct ProviderOwnerFact {
    /// Closed operation kind.
    pub kind: &'static str,
    /// Stable ledger ordinal.
    pub operation: u64,
    /// The provider's turn or session-workflow owner.
    pub owner: plakous::model::ModelOperationRoot,
    /// The answer marker occurs in this provider's committed evidence.
    pub text_marker: bool,
}

/// Reads every provider's owner and committed marker without opening a writer.
///
/// # Errors
/// Returns typed store lookup or integrity failures.
pub fn inspect_provider_owners(
    database: &Path,
    head: SessionLocator,
    answer: &str,
) -> Result<Vec<ProviderOwnerFact>, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    Ok(loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .iter()
        .filter_map(|op| {
            let Kind::Provider { root, .. } = op.kind() else {
                return None;
            };
            let text_marker = match op.state() {
                State::Completed(evidence) | State::Finalized(evidence) => evidence
                    .bytes()
                    .windows(answer.len())
                    .any(|window| window == answer.as_bytes()),
                _ => false,
            };
            Some(ProviderOwnerFact {
                kind: "provider",
                operation: op.operation_id(),
                owner: *root,
                text_marker,
            })
        })
        .collect())
}

/// Reads durable provider decisions for live-fact pilot assertions.
///
/// # Errors
/// Returns typed store or evidence-integrity failures, without opening a writer.
pub fn inspect_provider_decisions(
    database: &Path,
    head: SessionLocator,
) -> Result<Vec<String>, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    let mut decisions = Vec::new();
    for op in loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
    {
        if !matches!(op.kind(), Kind::Provider { .. }) {
            continue;
        }
        let (State::Completed(evidence) | State::Finalized(evidence)) = op.state() else {
            continue;
        };
        let (tag, value): (String, serde_json::Value) = serde_json::from_slice(evidence.bytes())
            .map_err(|_| plakous_agent::internal::SessionStoreError::corrupt())?;
        if tag == "provider-outcome-v2" {
            decisions.push(
                value
                    .get("decision")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(plakous_agent::internal::SessionStoreError::corrupt)?
                    .to_owned(),
            );
        }
    }
    Ok(decisions)
}

/// Reads committed scripted provider responses for live-fact pilot assertions.
///
/// # Errors
/// Returns typed store or evidence-integrity failures without opening a writer.
pub fn inspect_provider_responses(
    database: &Path,
    head: SessionLocator,
) -> Result<Vec<plakous::model::ModelResponse>, plakous_agent::internal::SessionStoreError> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    let mut responses = Vec::new();
    for op in loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
    {
        if !matches!(op.kind(), Kind::Provider { .. }) {
            continue;
        }
        let (State::Completed(evidence) | State::Finalized(evidence)) = op.state() else {
            continue;
        };
        let (tag, mut value): (String, serde_json::Value) =
            serde_json::from_slice(evidence.bytes())
                .map_err(|_| plakous_agent::internal::SessionStoreError::corrupt())?;
        // A round attempt's provider-outcome-v2 record carries its response
        // under `result`; a compaction attempt keeps its own evidence.
        let response = match tag.as_str() {
            "provider-outcome-v2" => value
                .get_mut("result")
                .and_then(|result| result.get_mut("Response"))
                .map(serde_json::Value::take),
            "completed" => Some(value),
            _ => None,
        };
        if let Some(response) = response {
            responses.push(
                serde_json::from_value(response)
                    .map_err(|_| plakous_agent::internal::SessionStoreError::corrupt())?,
            );
        }
    }
    Ok(responses)
}

fn root_answer_marker(kind: &Kind, evidence: &[u8], answer: &str) -> bool {
    matches!(
        kind,
        Kind::Provider {
            root: plakous::model::ModelOperationRoot::Turn(_),
            ..
        }
    ) && !answer.is_empty()
        && evidence
            .windows(answer.len())
            .any(|window| window == answer.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_workflow_markers_are_not_durable_root_answers()
    -> Result<(), Box<dyn std::error::Error>> {
        use plakous::model::{AttemptId, ModelOperationRoot, SessionWorkflowRootId, TurnId};
        let turn: TurnId = "00000000-0000-0000-0000-000000000001".parse()?;
        let attempt: AttemptId = "00000000-0000-0000-0000-000000000002".parse()?;
        let workflow = SessionWorkflowRootId::new([3; 32])?;
        let root = Kind::Provider {
            root: ModelOperationRoot::Turn(turn),
            attempt_id: attempt,
        };
        let summary = Kind::Provider {
            root: ModelOperationRoot::SessionWorkflow(workflow),
            attempt_id: attempt,
        };
        assert!(root_answer_marker(&root, b"fixture answer", "answer"));
        assert!(!root_answer_marker(&summary, b"fixture answer", "answer"));
        assert!(!root_answer_marker(
            &Kind::Command {
                turn_id: Some(turn)
            },
            b"fixture answer",
            "answer"
        ));
        assert!(!root_answer_marker(&root, b"fixture answer", ""));
        Ok(())
    }
}
