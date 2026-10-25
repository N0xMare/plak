//! Product submissions and their tree batches, in write-ahead order.

#[cfg(test)]
mod tests;

pub(super) async fn project_duplicate(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    locator: plakous::sessions::SessionLocator,
    command_id: &str,
) -> Result<bool, LedgerError> {
    let views = tokio::time::timeout(
        crate::runtime::host_loop::FINAL_PROJECTION_BOUND,
        crate::runtime::projection::project_to_tail(sessions, store, product_id, locator),
    )
    .await
    .map_err(|_| {
        LedgerError::Unavailable("duplicate settlement projector pass exceeded its bound".into())
    })?
    .map_err(|error| LedgerError::Unavailable(error.to_string()))?;
    accepted_duplicate_consumed(&views, command_id)
}

fn accepted_duplicate_consumed(
    views: &crate::sessions::projector::SessionViews,
    command_id: &str,
) -> Result<bool, LedgerError> {
    crate::driver::ledger::duplicate_consumed(
        if views.state.deliveries.contains_key(command_id) {
            crate::driver::ledger::AcceptanceFact::Present
        } else {
            crate::driver::ledger::AcceptanceFact::Absent
        },
        views.state.retained_from.unwrap_or(0),
        "accepted duplicate has no acceptance fact in the unpruned feed",
    )
}

use std::sync::Arc;

use plakous::model::SessionId;
use plakous::sessions::AgentSessions;

use crate::driver::ledger::{
    BatchRule, CommandIntent, CommandKind, CommandLedger, LedgerError, LedgerFuture, Prepared,
    Settlement,
};
use crate::runtime::TopologyRows;
use crate::session_store::SessionRecordStore;
use crate::sessions::projector::{Submission, SubmissionKind};
use crate::sessions::{QueueItem, ResultMailBatch, SessionRecord};

/// Records the exact batch a root command carries as a tree intent keyed by
/// its command id, before the product writes the submission (spec 03
/// section 8). Writing the same batch again is a no-op, so a resubmission
/// reuses it. Stored-only trees record the same intent through their rows.
fn record_root_batch(
    rows: &dyn TopologyRows,
    command_id: &str,
    command: &str,
    batch: &ResultMailBatch,
) -> Result<(), String> {
    let record = |tree: &mut crate::agent_tree::ProductAgentTree| {
        tree.record_command(crate::agent_tree::AgentCommand::intent(
            command_id,
            batch.recipient.clone(),
            None,
            crate::agent_tree::AgentCommandKind::RootBatch,
            command,
            batch.message_ids.clone(),
        ))
        .map_err(|error| error.message)
    };
    if let Some(runtime) = crate::topology_runtime::lookup(&batch.tree_id) {
        return runtime.mutate_then_emit(false, record);
    }
    let mut tree = rows
        .tree(&batch.tree_id)?
        .ok_or_else(|| "result tree is unavailable".to_owned())?;
    let revision = tree.revision;
    record(&mut tree)?;
    if tree.revision != revision {
        rows.save_tree(&mut tree)?;
    }
    Ok(())
}

fn withdraw_root_batch(
    rows: &dyn TopologyRows,
    command_id: &str,
    batch: &ResultMailBatch,
) -> Result<(), String> {
    let withdraw = |tree: &mut crate::agent_tree::ProductAgentTree| {
        tree.withdraw_command(command_id);
        Ok::<(), String>(())
    };
    if let Some(runtime) = crate::topology_runtime::lookup(&batch.tree_id) {
        return runtime.mutate_then_emit(false, withdraw);
    }
    let mut tree = rows
        .tree(&batch.tree_id)?
        .ok_or_else(|| "result tree is unavailable".to_owned())?;
    let revision = tree.revision;
    withdraw(&mut tree)?;
    if tree.revision != revision {
        rows.save_tree(&mut tree)?;
    }
    Ok(())
}

pub(super) struct RootLedger<'a> {
    rows: &'a (dyn TopologyRows + Sync),
    store: &'a dyn SessionRecordStore,
    projection_store: Option<&'a Arc<dyn SessionRecordStore>>,
    session: &'a mut SessionRecord,
    item: &'a QueueItem,
}

impl<'a> RootLedger<'a> {
    pub(super) fn new(
        rows: &'a (dyn TopologyRows + Sync),
        store: &'a dyn SessionRecordStore,
        session: &'a mut SessionRecord,
        item: &'a QueueItem,
    ) -> Self {
        Self {
            rows,
            store,
            projection_store: None,
            session,
            item,
        }
    }

    pub(super) fn with_projection(mut self, store: &'a Arc<dyn SessionRecordStore>) -> Self {
        self.projection_store = Some(store);
        self
    }
}

impl CommandLedger for RootLedger<'_> {
    fn prepare(&mut self, intent: CommandIntent<'_>) -> Result<Prepared, LedgerError> {
        let (kind, target_turn_id) = match intent.kind {
            CommandKind::RootStart => (SubmissionKind::RootStart, None),
            CommandKind::RootSteer(turn) => (SubmissionKind::SendNowSteer, Some(turn.to_string())),
            CommandKind::HostWake | CommandKind::Resume => (SubmissionKind::HostWake, None),
            _ => {
                return Err(LedgerError::Integrity(
                    "child command passed to root ledger".into(),
                ));
            }
        };
        let batch = match intent.batch {
            BatchRule::None => None,
            BatchRule::RootResults(batch) => Some(batch),
            _ => {
                return Err(LedgerError::Integrity(
                    "child batch passed to root ledger".into(),
                ));
            }
        };
        let submission = Submission {
            command_id: intent.command_id.to_owned(),
            kind,
            queue_item_id: self.item.id.clone(),
            queue_item_revision: self.item.revision,
            target_turn_id,
            canonical_command: intent.base_text.to_owned(),
            submitted_at_ms: self.item.submitted_at_ms,
        };
        if kind == SubmissionKind::SendNowSteer {
            if batch.is_some() {
                return Err(LedgerError::Integrity(
                    "root steer cannot carry a tree batch".into(),
                ));
            }
            // A live steer writes in place, including an exact retry. It must
            // neither touch the session nor reject an already-pending intent.
            self.store.save_submission(self.session, &submission)?;
            return Ok(Prepared {
                prompt: intent.base_text.to_owned(),
                message_ids: Vec::new(),
                instruction: false,
            });
        }
        // Only batch preparation pre-reads and touches the record.
        let batch_preparation = batch.is_some() || kind == SubmissionKind::HostWake;
        if batch_preparation
            && self
                .store
                .load_intents(&self.session.id)
                .map_err(LedgerError::Unavailable)?
                .submission(intent.command_id)
                .is_some()
        {
            return Err(LedgerError::Integrity(
                "result delivery must reuse its existing immutable submission".into(),
            ));
        }
        let mut candidate = self.session.clone();
        if let Some(batch) = &batch {
            record_root_batch(self.rows, intent.command_id, intent.base_text, batch)
                .map_err(LedgerError::Unavailable)?;
        }
        if batch_preparation {
            candidate.touch();
        }
        let saved = self.store.save_submission(&mut candidate, &submission);
        if let Err(error) = saved {
            if error.refusal().is_some()
                && let Some(batch) = &batch
            {
                withdraw_root_batch(self.rows, intent.command_id, batch)
                    .map_err(LedgerError::Unavailable)?;
            }
            return Err(error.into());
        }
        *self.session = candidate;
        Ok(Prepared {
            prompt: intent.base_text.to_owned(),
            message_ids: batch.map_or_else(Vec::new, |batch| batch.message_ids),
            instruction: intent.kind.is_instruction(),
        })
    }

    fn settle(&mut self, command_id: &str, settlement: Settlement) -> Result<(), LedgerError> {
        match settlement {
            Settlement::Recorded => Ok(()),
            Settlement::Consumed => self
                .store
                .discard_submission(self.session, command_id, None)
                .map_err(LedgerError::Store),
            Settlement::Refused { pause } => self
                .store
                .discard_submission(self.session, command_id, pause)
                .map_err(LedgerError::Store),
        }
    }

    fn duplicate_consumed<'a>(
        &'a self,
        sessions: &'a AgentSessions,
        session: SessionId,
        command_id: &'a str,
    ) -> LedgerFuture<'a> {
        Box::pin(async move {
            let projection_store = self.projection_store.ok_or_else(|| {
                LedgerError::Integrity("duplicate settlement requires a projection store".into())
            })?;
            let locator = self.session.plakous_locator.ok_or_else(|| {
                LedgerError::Integrity("submission has no runtime locator".into())
            })?;
            if locator.session_id() != session {
                return Err(LedgerError::Integrity(
                    "duplicate session does not match submission".into(),
                ));
            }
            project_duplicate(
                sessions,
                projection_store,
                &self.session.id,
                locator,
                command_id,
            )
            .await
        })
    }
}
