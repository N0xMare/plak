//! The one pure fact function: a mutation's facts from its checkpoints.
//!
//! A commit's facts are exactly its own semantic delta between the parent and
//! successor checkpoints, so a fact never describes work the checkpoint does
//! not hold, and recovery commits state their closure in the vocabulary of
//! live work.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use plakous_model::{
    CallId, CanonicalHistory, CheckpointId, ContentBlock, HistorySegment, HistorySegmentKind,
    TurnId,
};

use super::{
    FeedCallerLineage, FeedCompactReason, FeedFact, FeedLocator, FeedOperationKind,
    FeedOperationState, FeedWorkflowKind, FeedWorkflowOutcome,
};
use crate::manifest::{RuntimeContextTransitionKind, RuntimeOperationRoot};
use crate::runtime::feed_views::{self, CommandOperation};
use crate::session::{BranchTransitionMode, SessionWorkflowId};
use crate::{
    AgentError, AgentErrorKind, CommandId, DurableCommandDisposition, DurableCommandRecord,
    DurableOperationKind, DurableOperationRecord, DurableOperationState, RuntimeCheckpoint,
    RuntimeLifecycle, RuntimeWorkingRoot,
};

/// The facts of a create: the session and whether its genesis was seeded.
/// A seeded genesis's history is the session's starting point, not an append.
pub(crate) fn genesis_facts(operation_id: &str, genesis: &RuntimeCheckpoint) -> Vec<FeedFact> {
    vec![FeedFact::SessionCreated {
        operation_id: operation_id.to_owned(),
        seeded: !genesis.manifest().canonical().history().is_empty(),
    }]
}

/// The facts of a fork or rebind, carried by the target branch's record: the
/// workflow is admitted and settled by this one publication.
pub(crate) fn branch_facts(
    workflow_id: SessionWorkflowId,
    operation_id: &str,
    transition: BranchTransitionMode,
    target: &RuntimeCheckpoint,
) -> Vec<FeedFact> {
    let kind = match transition {
        BranchTransitionMode::OrdinaryFork => FeedWorkflowKind::Fork,
        BranchTransitionMode::Rebind => FeedWorkflowKind::Rebind,
    };
    vec![
        FeedFact::WorkflowAdmitted {
            workflow_id,
            operation_id: Some(operation_id.to_owned()),
            kind,
        },
        FeedFact::WorkflowSettled {
            workflow_id,
            operation_id: Some(operation_id.to_owned()),
            kind,
            outcome: FeedWorkflowOutcome::Published {
                target: locator(target),
            },
        },
    ]
}

/// The facts of one commit from `parent` to `successor`, in the order a
/// consumer applies them: commands, admitted workflows, history, operations,
/// the turn terminal, settled workflows.
///
/// # Errors
///
/// Refuses ledger records whose exact encodings this build cannot read.
pub(crate) fn commit_facts(
    parent: &RuntimeCheckpoint,
    successor: &Arc<RuntimeCheckpoint>,
) -> Result<Vec<FeedFact>, AgentError> {
    let mut facts = command_facts(parent, successor)?;
    let operations = advanced_operations(parent, successor);
    let checkpoint_id = successor.id();
    let mut settled = Vec::new();
    for record in &operations {
        if let DurableOperationKind::Compaction { workflow_id } = record.kind() {
            let view = feed_views::compaction_view(record)?;
            let reason = if view.turn_fence {
                FeedCompactReason::TurnFence
            } else {
                FeedCompactReason::Endpoint
            };
            let kind = FeedWorkflowKind::Compact { reason };
            let operation_id = match reason {
                FeedCompactReason::Endpoint => Some(view.operation_id.clone()),
                FeedCompactReason::TurnFence => None,
            };
            if record.transitions()[0].checkpoint_id() == checkpoint_id {
                facts.push(FeedFact::WorkflowAdmitted {
                    workflow_id: *workflow_id,
                    operation_id: operation_id.clone(),
                    kind,
                });
            }
            if finalized_here(record, checkpoint_id) {
                settled.push(FeedFact::WorkflowSettled {
                    workflow_id: *workflow_id,
                    operation_id,
                    kind,
                    outcome: compaction_outcome(parent, successor, record, &view)?,
                });
            }
        }
    }
    let turn_id = owning_turn(parent.manifest().lifecycle())
        .or_else(|| owning_turn(successor.manifest().lifecycle()));
    let history = history_facts(
        parent.manifest().canonical().history(),
        successor.manifest().canonical().history(),
        turn_id,
    );
    let closed_exchange = history.iter().any(|fact| {
        matches!(
            fact,
            FeedFact::HistoryAppended {
                segment: HistorySegmentKind::ToolExchange,
                ..
            }
        )
    });
    facts.extend(history);
    for record in &operations {
        if let Some(kind) = operation_kind(parent, successor, record) {
            for transition in record.transitions() {
                if transition.checkpoint_id() != checkpoint_id {
                    continue;
                }
                if let Some(state) = reported_state(transition.state()) {
                    facts.push(FeedFact::OperationAdvanced {
                        operation_id: record.operation_id(),
                        turn_id: operation_turn(record),
                        kind: kind.clone(),
                        state,
                    });
                }
            }
        }
    }
    facts.extend(terminal_facts(
        parent,
        successor,
        &operations,
        closed_exchange,
    )?);
    facts.extend(settled);
    Ok(facts)
}

fn command_facts(
    parent: &RuntimeCheckpoint,
    successor: &RuntimeCheckpoint,
) -> Result<Vec<FeedFact>, AgentError> {
    let ledger = successor.manifest().ledger();
    let appended: Vec<&DurableCommandRecord> = ledger
        .command_delta_since(parent.manifest().ledger())
        .unwrap_or_else(|| {
            let last = parent
                .manifest()
                .ledger()
                .commands()
                .last()
                .map_or(0, |record| record.sequence());
            ledger
                .commands()
                .into_iter()
                .filter(|record| record.sequence() > last)
                .collect()
        });
    appended
        .into_iter()
        .map(|record| {
            let view = feed_views::command_view(record)?;
            let command_id = CommandId::new(record.command_id())
                .map_err(|_| integrity_error("feed command identity is invalid"))?;
            Ok(match (record.disposition(), view.rejection) {
                (DurableCommandDisposition::Accepted, None) => FeedFact::CommandAccepted {
                    command_id,
                    kind: view.kind,
                    turn_id: record.turn_id(),
                    sequence: record.sequence(),
                    operation_id: feed_views::command_operation_id(successor, record),
                },
                (DurableCommandDisposition::Rejected, Some(error)) => FeedFact::CommandRejected {
                    command_id,
                    kind: view.kind,
                    error,
                },
                _ => return Err(integrity_error("feed command disposition is inconsistent")),
            })
        })
        .collect()
}

/// The records this commit changed, each once, in operation order.
fn advanced_operations<'a>(
    parent: &RuntimeCheckpoint,
    successor: &'a RuntimeCheckpoint,
) -> Vec<&'a DurableOperationRecord> {
    let ledger = successor.manifest().ledger();
    let checkpoint_id = successor.id();
    let changed: BTreeMap<u64, &DurableOperationRecord> = ledger
        .operation_delta_since(parent.manifest().ledger())
        .map_or_else(
            || ledger.operations(),
            |delta| {
                delta
                    .into_iter()
                    .map(crate::manifest::OperationLedgerMutationRef::record)
                    .collect()
            },
        )
        .into_iter()
        .map(|record| (record.operation_id(), record))
        .collect();
    changed
        .into_values()
        .filter(|record| {
            record
                .transitions()
                .iter()
                .any(|transition| transition.checkpoint_id() == checkpoint_id)
        })
        .collect()
}

fn reported_state(state: &DurableOperationState) -> Option<FeedOperationState> {
    match state {
        DurableOperationState::Dispatched => Some(FeedOperationState::Dispatched),
        DurableOperationState::Completed(_) => Some(FeedOperationState::Completed),
        DurableOperationState::OutcomeUnknown(_) => Some(FeedOperationState::OutcomeUnknown),
        DurableOperationState::ProvenNotDispatched(_) => {
            Some(FeedOperationState::ProvenNotDispatched)
        }
        DurableOperationState::Finalized(_) => Some(FeedOperationState::Finalized),
        DurableOperationState::Prepared
        | DurableOperationState::DecisionRequired(_)
        | DurableOperationState::ContinuationRequired(_)
        | DurableOperationState::Claimed(_)
        | DurableOperationState::LocatorBound(_) => None,
    }
}

fn operation_kind(
    parent: &RuntimeCheckpoint,
    successor: &RuntimeCheckpoint,
    record: &DurableOperationRecord,
) -> Option<FeedOperationKind> {
    Some(match record.kind() {
        DurableOperationKind::Provider { attempt_id, .. } => FeedOperationKind::ProviderAttempt {
            attempt_id: *attempt_id,
        },
        // A call block this build cannot read names no tool, so it yields no
        // fact rather than refusing the commit that closes it.
        DurableOperationKind::Effect { call_id, .. } => {
            let Ok(effect) = feed_views::effect_view(record) else {
                return None;
            };
            // A nested call's block names its evaluation, not the outer
            // marker; the ledger's evaluation record holds that.
            let lineage = match effect.lineage {
                FeedCallerLineage::LocalProgrammatic {
                    evaluation_operation_id,
                    parent_call_id: None,
                    cell_id,
                    request_sequence,
                    depth,
                } => FeedCallerLineage::LocalProgrammatic {
                    evaluation_operation_id,
                    parent_call_id: evaluation_call(successor, evaluation_operation_id)
                        .or_else(|| evaluation_call(parent, evaluation_operation_id)),
                    cell_id,
                    request_sequence,
                    depth,
                },
                lineage => lineage,
            };
            FeedOperationKind::Effect {
                call_id: *call_id,
                exposed_name: effect.exposed_name,
                lineage,
                subject: effect.subject,
            }
        }
        DurableOperationKind::CompositionChild {
            call_id,
            evaluation_operation_id,
            cell_id,
            request_sequence,
            ..
        } => {
            let Ok(effect) = feed_views::nested_effect_view(record) else {
                return None;
            };
            FeedOperationKind::Effect {
                call_id: *call_id,
                exposed_name: effect.exposed_name,
                lineage: FeedCallerLineage::LocalProgrammatic {
                    evaluation_operation_id: *evaluation_operation_id,
                    parent_call_id: evaluation_call(successor, *evaluation_operation_id)
                        .or_else(|| evaluation_call(parent, *evaluation_operation_id)),
                    cell_id: cell_id.clone(),
                    request_sequence: *request_sequence,
                    depth: feed_views::LOCAL_COMPOSITION_DEPTH,
                },
                subject: effect.subject,
            }
        }
        DurableOperationKind::ProcessSession { session_id, .. } => {
            let job_number = [successor, parent].into_iter().find_map(|checkpoint| {
                checkpoint
                    .manifest()
                    .jobs()
                    .rows()
                    .iter()
                    .find(|row| row.session_id == *session_id)
                    .map(|row| row.number)
            });
            FeedOperationKind::ProcessSession { job_number }
        }
        DurableOperationKind::CompositionEvaluation { call_id, .. } => {
            let effect = feed_views::code_cell_view(record).ok()?;
            FeedOperationKind::CodeCell {
                call_id: *call_id,
                exposed_name: effect.exposed_name,
                subject: effect.subject,
            }
        }
        DurableOperationKind::Compaction { workflow_id } => FeedOperationKind::Compaction {
            workflow_id: *workflow_id,
        },
        DurableOperationKind::Command { .. }
        | DurableOperationKind::EffectDecision { .. }
        | DurableOperationKind::ToolBatchContinuation { .. }
        | DurableOperationKind::ServerPauseContinuation { .. }
        | DurableOperationKind::CompositionContinuation { .. }
        | DurableOperationKind::Commit { .. } => return None,
    })
}

fn operation_turn(record: &DurableOperationRecord) -> Option<TurnId> {
    match record.kind() {
        DurableOperationKind::Command { turn_id } => *turn_id,
        DurableOperationKind::Provider { root, .. } | DurableOperationKind::Commit { root } => {
            root.turn_id()
        }
        DurableOperationKind::EffectDecision { turn_id, .. }
        | DurableOperationKind::Effect { turn_id, .. }
        | DurableOperationKind::ToolBatchContinuation { turn_id, .. }
        | DurableOperationKind::ServerPauseContinuation { turn_id, .. }
        | DurableOperationKind::CompositionEvaluation { turn_id, .. }
        | DurableOperationKind::CompositionContinuation { turn_id, .. }
        | DurableOperationKind::CompositionChild { turn_id, .. }
        | DurableOperationKind::ProcessSession { turn_id, .. } => Some(*turn_id),
        DurableOperationKind::Compaction { .. } => None,
    }
}

fn owning_turn(lifecycle: &RuntimeLifecycle) -> Option<TurnId> {
    match lifecycle {
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id, .. },
            ..
        } => Some(*turn_id),
        RuntimeLifecycle::Quiescent
        | RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::SessionWorkflow(_),
            ..
        } => None,
    }
}

fn finalized_here(record: &DurableOperationRecord, checkpoint_id: CheckpointId) -> bool {
    record.transitions().iter().any(|transition| {
        transition.checkpoint_id() == checkpoint_id
            && matches!(transition.state(), DurableOperationState::Finalized(_))
    })
}

/// A compaction's settlement, classified from its own finalization evidence:
/// a published successor is `Compacted`; a commit that closes a lost owner's
/// workflow (the abandonment evidence checkpoint, or an in-turn fold closed
/// with its abandoned turn) is `Abandoned`; any other recorded failure is a
/// live `Failed`.
fn compaction_outcome(
    parent: &RuntimeCheckpoint,
    successor: &RuntimeCheckpoint,
    record: &DurableOperationRecord,
    view: &feed_views::CompactionView,
) -> Result<FeedWorkflowOutcome, AgentError> {
    let DurableOperationKind::Compaction { workflow_id } = record.kind() else {
        return Err(integrity_error(
            "feed compaction record is not a compaction",
        ));
    };
    let transition = successor.manifest().canonical().context().transition();
    if let Some(transition) = transition.filter(|transition| {
        transition.kind() == RuntimeContextTransitionKind::SuccessfulCompaction
            && parent.manifest().canonical().context().transition() != Some(transition)
    }) {
        let tokens_after = view
            .tokens_after
            .ok_or_else(|| integrity_error("feed compaction success has no after-measurement"))?;
        return Ok(FeedWorkflowOutcome::Compacted {
            source: FeedLocator {
                session_id: transition.source_session(),
                branch_id: transition.source_branch(),
                checkpoint_id: transition.source_checkpoint(),
            },
            target: locator(successor),
            pass: view.pass,
            selected_segments: count(replaced_segments(
                parent.manifest().canonical().history(),
                successor.manifest().canonical().history(),
            )),
            tokens_before: view.tokens_before,
            tokens_after,
        });
    }
    let summary = successor
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|candidate| {
            matches!(
                candidate.kind(),
                DurableOperationKind::Provider {
                    root: RuntimeOperationRoot::SessionWorkflow(root),
                    ..
                } if root == workflow_id
            )
        });
    let Some(summary) = summary else {
        return Ok(FeedWorkflowOutcome::NoEligibleRange);
    };
    let reached = |matches: fn(&DurableOperationState) -> bool| {
        summary
            .transitions()
            .iter()
            .any(|transition| matches(transition.state()))
    };
    // Neither a bound recovery locator nor crossing the dispatch fence is
    // sending: a start refused after either records ProvenNotDispatched, which
    // proves the summary unsent.
    let sent = reached(|state| {
        matches!(
            state,
            DurableOperationState::OutcomeUnknown(_) | DurableOperationState::Completed(_)
        )
    });
    let proven_unsent =
        reached(|state| matches!(state, DurableOperationState::ProvenNotDispatched(_))) && !sent;
    let closure = [record, summary].into_iter().any(|operation| {
        operation
            .transitions()
            .iter()
            .any(|transition| transition.checkpoint_id() == view.abandonment_checkpoint)
    }) || feed_views::operation_abandoned_at(successor, record.operation_id())?;
    if closure {
        return Ok(FeedWorkflowOutcome::Abandoned {
            summary_unsent: proven_unsent,
        });
    }
    let failure = feed_views::compaction_failure(record)?;
    Ok(FeedWorkflowOutcome::Failed {
        summary_sent: !proven_unsent,
        kind: failure.kind(),
        model_kind: failure.model_kind(),
    })
}

/// Source segments the successor does not hold by identity: the ones a pass
/// replaced or rewrote.
fn replaced_segments(parent: &CanonicalHistory, successor: &CanonicalHistory) -> usize {
    let kept: HashSet<_> = successor
        .segments()
        .iter()
        .map(HistorySegment::identity)
        .collect();
    parent
        .segments()
        .iter()
        .filter(|segment| !kept.contains(&segment.identity()))
        .count()
}

/// The history facts of one commit. Each successor segment after the longest
/// common prefix that holds an entry the parent did not is appended; one that
/// the parent held by identity moved (a compaction's retained tail); one whose
/// entries the parent all held was rewritten in place, and every rewrite of
/// the commit is reported by one bounded `HistoryRewritten`.
fn history_facts(
    parent: &CanonicalHistory,
    successor: &CanonicalHistory,
    turn_id: Option<TurnId>,
) -> Vec<FeedFact> {
    let mut facts = Vec::new();
    let mut push = |segment: &HistorySegment, first_ordinal: usize| {
        let items = segment.items();
        let (Some(first), Some(last)) = (items.first(), items.last()) else {
            return;
        };
        facts.push(FeedFact::HistoryAppended {
            turn_id,
            segment: segment.kind(),
            first_ordinal: count(first_ordinal),
            count: count(items.len()),
            first_entry: first.id(),
            last_entry: last.id(),
            tool_results: items
                .iter()
                .flat_map(plakous_model::ConversationItem::blocks)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult(result) => Some(result.call_id()),
                    _ => None,
                })
                .collect(),
        });
    };
    if let Some(appended) = successor.append_delta_since(parent) {
        let mut ordinal = parent.item_count();
        for segment in &appended {
            push(segment, ordinal);
            ordinal += segment.items().len();
        }
        return facts;
    }
    let parent_segments = parent.segments();
    let mut ordinal = 0;
    let mut common = 0;
    for (index, segment) in successor.segments().iter().enumerate() {
        if parent_segments
            .get(index)
            .is_none_or(|held| held.identity() != segment.identity())
        {
            break;
        }
        common = index + 1;
        ordinal += segment.items().len();
    }
    let held: HashSet<_> = parent_segments
        .iter()
        .skip(common)
        .map(HistorySegment::identity)
        .collect();
    let entries: HashSet<_> = parent_segments
        .iter()
        .skip(common)
        .flat_map(HistorySegment::items)
        .map(plakous_model::ConversationItem::id)
        .collect();
    let mut rewritten: Option<(usize, usize)> = None;
    for segment in successor.segments().iter().skip(common) {
        let len = segment.items().len();
        if !held.contains(&segment.identity()) {
            if segment
                .items()
                .iter()
                .all(|item| entries.contains(&item.id()))
            {
                let first = rewritten.map_or(ordinal, |(first, _)| first);
                rewritten = Some((first, ordinal + len));
            } else {
                push(segment, ordinal);
            }
        }
        ordinal += len;
    }
    if let Some((first, end)) = rewritten.filter(|(first, end)| end > first) {
        facts.push(FeedFact::HistoryRewritten {
            turn_id,
            first_ordinal: count(first),
            count: count(end - first),
        });
    }
    facts
}

fn terminal_facts(
    parent: &RuntimeCheckpoint,
    successor: &Arc<RuntimeCheckpoint>,
    operations: &[&DurableOperationRecord],
    closed_exchange: bool,
) -> Result<Vec<FeedFact>, AgentError> {
    let checkpoint_id = successor.id();
    let mut terminals = Vec::new();
    let mut steering_settled = false;
    for record in operations {
        if !matches!(
            record.kind(),
            DurableOperationKind::Command { turn_id: Some(_) }
        ) || !finalized_here(record, checkpoint_id)
        {
            continue;
        }
        if parent
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .any(|prior| {
                prior.operation_id() == record.operation_id()
                    && matches!(prior.state(), DurableOperationState::Finalized(_))
            })
        {
            continue;
        }
        match feed_views::command_operation(parent, successor, record)? {
            CommandOperation::RootTerminal(terminal) => terminals.push(terminal),
            CommandOperation::SteerNotApplied => steering_settled = true,
            CommandOperation::Other => {}
        }
    }
    Ok(terminals
        .into_iter()
        .map(|terminal| FeedFact::TurnTerminal {
            turn_id: terminal.turn_id,
            command_id: terminal.command_id,
            outcome: terminal.outcome,
            closed_exchange,
            steering_settled_unapplied: steering_settled,
            usage: terminal.usage,
        })
        .collect())
}

/// The outer call of the Code Cell evaluation `operation_id`, while the
/// ledger retains its record.
fn evaluation_call(checkpoint: &RuntimeCheckpoint, operation_id: u64) -> Option<CallId> {
    checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| record.operation_id() == operation_id)
        .and_then(|record| match record.kind() {
            DurableOperationKind::CompositionEvaluation { call_id, .. } => Some(*call_id),
            _ => None,
        })
}

fn locator(checkpoint: &RuntimeCheckpoint) -> FeedLocator {
    let topology = checkpoint.manifest().topology();
    FeedLocator {
        session_id: topology.session_id,
        branch_id: topology.branch_id,
        checkpoint_id: topology.checkpoint_id,
    }
}

fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn integrity_error(message: &'static str) -> AgentError {
    AgentError::new(AgentErrorKind::Session, message)
}

#[cfg(test)]
mod tests;
