//! The one recovery predicate (spec 01 §5.5): a pure decision over the exact
//! head checkpoint. The live open, the read-only `SQLite` inspection and the
//! rehearsal differ only in where they load the evidence from.

use super::{RootClosure, newest_root_turn_closure};
use crate::runtime::continuation::decode_restored_continuation;
use crate::runtime::durable::workflow_closure::{WorkflowClosure, workflow_compaction_closure};
use crate::{
    AgentError, AgentErrorKind, RuntimeCheckpoint, RuntimeLifecycle, WorkingRecoveryPhase,
};

/// The exact head checkpoint: its ledger, its continuation and
/// effect-decision frames, and its canonical history. Nothing outside the
/// runtime's own checkpoint enters the verdict.
#[derive(Clone, Copy)]
pub(in crate::runtime) struct RecoveryEvidence<'a> {
    head: &'a RuntimeCheckpoint,
}

impl<'a> RecoveryEvidence<'a> {
    pub(in crate::runtime) const fn head(head: &'a RuntimeCheckpoint) -> Self {
        Self { head }
    }
}

/// What a newly opened owner does with a head, and what every read-only caller
/// predicts it does.
pub(in crate::runtime) enum RecoveryVerdict {
    Quiescent,
    /// An ambiguous exact commit is recovered before anything else.
    RecoverCommit,
    /// A restorable frame (effect decision, hosted server pause or tool
    /// batch), or a release-owned working scope an explicit facade workflow
    /// names. Not a closure.
    Restore(WorkingRecoveryPhase),
    /// The closure-table row this head matches, published by the open.
    Close(Box<RootClosure>),
    /// The session-workflow compaction row this head matches, published by
    /// the open with the workflow's `WorkflowSettled` fact.
    SettleWorkflow(Box<WorkflowClosure>),
    /// Typed integrity: a live driver of this build never writes this head.
    Refuse(RecoveryRefusal),
}

pub(in crate::runtime) enum RecoveryRefusal {
    /// A record whose evidence does not decode, or a shape no row admits.
    Integrity(AgentError),
}

impl RecoveryRefusal {
    pub(in crate::runtime) fn into_error(self) -> AgentError {
        match self {
            Self::Integrity(error) => error,
        }
    }
}

impl RecoveryVerdict {
    pub(in crate::runtime) const fn closes(&self) -> bool {
        matches!(self, Self::Close(_) | Self::SettleWorkflow(_))
    }

    pub(in crate::runtime) const fn refuses(&self) -> bool {
        matches!(self, Self::Refuse(_))
    }

    pub(in crate::runtime) fn into_closure(self) -> Result<Option<RootClosure>, AgentError> {
        match self {
            Self::Close(closure) => Ok(Some(*closure)),
            Self::Refuse(refusal) => Err(refusal.into_error()),
            Self::Quiescent | Self::RecoverCommit | Self::Restore(_) | Self::SettleWorkflow(_) => {
                Ok(None)
            }
        }
    }
}

/// The single production recovery decision.
pub(in crate::runtime) fn recovery_verdict(evidence: RecoveryEvidence<'_>) -> RecoveryVerdict {
    let head = evidence.head;
    let RuntimeLifecycle::Working { recovery_phase, .. } = head.manifest().lifecycle() else {
        return RecoveryVerdict::Quiescent;
    };
    if *recovery_phase == WorkingRecoveryPhase::CommitRecovery {
        return RecoveryVerdict::RecoverCommit;
    }
    match workflow_compaction_closure(head) {
        Ok(Some(closure)) => return RecoveryVerdict::SettleWorkflow(Box::new(closure)),
        Ok(None) => {}
        Err(error) => return RecoveryVerdict::Refuse(RecoveryRefusal::Integrity(error)),
    }
    let refusal = match newest_root_turn_closure(head) {
        Ok(Some(closure)) => return RecoveryVerdict::Close(Box::new(closure)),
        Ok(None) => {
            return RecoveryVerdict::Refuse(RecoveryRefusal::Integrity(AgentError::new(
                AgentErrorKind::Internal,
                "a working head has no closure",
            )));
        }
        Err(error) if error.kind() == AgentErrorKind::OutcomeUnknown => error,
        Err(error) => return RecoveryVerdict::Refuse(RecoveryRefusal::Integrity(error)),
    };
    if decode_restored_continuation(head).is_ok()
        || crate::recovery_scope::in_release_scope(head, *recovery_phase)
    {
        return RecoveryVerdict::Restore(*recovery_phase);
    }
    RecoveryVerdict::Refuse(RecoveryRefusal::Integrity(refusal))
}

/// The one recovery predicate's verdict over an exact head (spec 01 §5.5),
/// for read-only callers that load the same evidence an open does.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointRecoveryVerdict {
    Quiescent,
    RecoverCommit,
    /// A restorable frame or a release-owned working scope; not a closure.
    Restore(WorkingRecoveryPhase),
    /// A closure that finishes the turn from its recorded terminal.
    Finish,
    /// A closure that abandons the turn's unfinished work.
    Abandon,
    /// A closure that settles an admitted session-workflow compaction.
    SettleWorkflow,
    /// Typed integrity: no row admits this head.
    Refuse,
}

/// Classifies the head through the one recovery predicate.
#[doc(hidden)]
#[must_use]
pub fn checkpoint_recovery_verdict(checkpoint: &RuntimeCheckpoint) -> CheckpointRecoveryVerdict {
    match recovery_verdict(RecoveryEvidence::head(checkpoint)) {
        RecoveryVerdict::Quiescent => CheckpointRecoveryVerdict::Quiescent,
        RecoveryVerdict::RecoverCommit => CheckpointRecoveryVerdict::RecoverCommit,
        RecoveryVerdict::Restore(phase) => CheckpointRecoveryVerdict::Restore(phase),
        RecoveryVerdict::Close(closure) => match *closure {
            RootClosure::Abandon { .. }
            | RootClosure::FinalizeRecorded {
                code_cell: Some(_), ..
            } => CheckpointRecoveryVerdict::Abandon,
            RootClosure::FinalizeRecorded { .. } | RootClosure::Complete(_) => {
                CheckpointRecoveryVerdict::Finish
            }
        },
        RecoveryVerdict::SettleWorkflow(_) => CheckpointRecoveryVerdict::SettleWorkflow,
        RecoveryVerdict::Refuse(_) => CheckpointRecoveryVerdict::Refuse,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sources(dir: &Path, out: &mut Vec<(String, String)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                sources(&path, out)?;
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let relative = path
                    .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                out.push((relative, std::fs::read_to_string(&path)?));
            }
        }
        Ok(())
    }

    /// `recovery_verdict` is the only production decision about how a
    /// working head recovers: every other caller asks it, and the closure
    /// folds it is built from are called from nowhere else.
    #[test]
    fn the_recovery_verdict_is_the_only_production_decision_point() -> TestResult {
        let mut files = Vec::new();
        sources(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        )?;
        let owner = "src/runtime/durable/turn_recovery.rs";
        let verdict = "src/runtime/durable/turn_recovery/verdict.rs";
        let rules: &[(&str, &[&str])] = &[
            ("newest_root_turn_closure(", &[owner, verdict]),
            ("recovery_verdict(", &[owner, verdict]),
            (
                "workflow_compaction_closure(",
                &[verdict, "src/runtime/durable/workflow_closure.rs"],
            ),
            ("terminal_evidence_fold(", &[owner, verdict]),
            ("abandonment_evidence_fold(", &[owner, verdict]),
            ("local_terminal_from_ledger(", &[owner, verdict]),
        ];
        for (needle, allowed) in rules {
            for (path, text) in &files {
                if path.contains("test") {
                    continue;
                }
                if text.contains(needle) && !allowed.contains(&path.as_str()) {
                    return Err(format!("{path} calls {needle} outside the predicate").into());
                }
            }
        }
        Ok(())
    }
}
