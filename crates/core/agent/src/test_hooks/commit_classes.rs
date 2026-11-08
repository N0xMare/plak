//! Content-free classes for representative indexed crash cuts.

use crate::{DurableOperationKind as Kind, DurableOperationState as State, RuntimeCheckpoint};
use std::collections::BTreeSet;

/// Classifies this publication's transitions and retained anchor classes.
/// The second value is the authenticated retired-attempt counter, not an
/// operation ordinal. No identities, request bytes or evidence enter the trace.
#[must_use]
pub fn checkpoint_commit_classes(checkpoint: &RuntimeCheckpoint) -> (Vec<String>, u64) {
    let ledger = checkpoint.manifest().ledger();
    let records = ledger.operations();
    let provider = records
        .iter()
        .rev()
        .find(|record| {
            matches!(
                record.kind(),
                Kind::Provider {
                    root: plakous_model::ModelOperationRoot::Turn(_),
                    ..
                }
            )
        })
        .map_or(0, |record| record.operation_id());
    let mut classes = BTreeSet::new();
    if checkpoint
        .manifest()
        .jobs()
        .rows()
        .iter()
        .any(|job| job.terminal.is_none())
    {
        classes.insert("anchor:live-job".into());
    }
    for record in records {
        let kind = kind(record.kind());
        let state = state(record.state());
        if record
            .transitions()
            .last()
            .is_some_and(|transition| transition.checkpoint_id() == checkpoint.id())
        {
            classes.insert(format!("change:{kind}:{state}"));
        }
        let side = if record.operation_id() < provider {
            "below-p"
        } else {
            "at-or-above-p"
        };
        classes.insert(format!("anchor:{kind}:{state}:{side}"));
    }
    (
        classes.into_iter().collect(),
        ledger.retired_counter().map_or(0, |(_, count)| count),
    )
}

fn kind(kind: &Kind) -> &'static str {
    match kind {
        Kind::Command { .. } => "command",
        Kind::Provider { .. } => "provider",
        Kind::EffectDecision { .. } => "decision",
        Kind::Effect { .. } => "effect",
        Kind::ToolBatchContinuation { .. } => "batch",
        Kind::ServerPauseContinuation { .. } => "pause",
        Kind::CompositionEvaluation { .. } => "evaluation",
        Kind::CompositionContinuation { .. } => "continuation",
        Kind::CompositionChild { .. } => "child",
        Kind::ProcessSession { .. } => "process",
        Kind::Commit { .. } => "commit",
        Kind::Compaction { .. } => "fold",
    }
}

fn state(state: &State) -> &'static str {
    match state {
        State::Prepared => "prepared",
        State::Dispatched => "dispatched",
        State::Claimed(_) => "claimed",
        State::LocatorBound(_) => "locator-bound",
        State::Completed(_) => "completed",
        State::Finalized(_) => "finalized",
        State::ProvenNotDispatched(_) => "unsent",
        State::OutcomeUnknown(_) => "unknown",
        State::DecisionRequired(_) => "decision",
        State::ContinuationRequired(_) => "continuation",
    }
}
