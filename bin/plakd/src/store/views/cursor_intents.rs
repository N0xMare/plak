//! Host intents written at the session's applied projector cursor: the
//! turn-free controls and the root's loss intents.

use plakous_hosted::session_store::RecordStoreError;
use plakous_hosted::sessions::projector::{HostPauseCause, RootLossIntent};
use rusqlite::{Transaction, TransactionBehavior};

use super::super::{ProductStore, write_error};
use super::{PAUSE_CAUSE, ROOT_CONTINUATION, ROOT_INTERRUPT, WAKE_SUPPRESSION};

impl ProductStore {
    pub fn write_host_controls(
        &mut self,
        session_id: &str,
        suppress_wake: bool,
        pause: Option<HostPauseCause>,
    ) -> Result<(), RecordStoreError> {
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| write_error(error, "could not start host intent write".to_owned()))?;
        let (anchor, ordinal) = cursor_slot(&tx, session_id)?;
        if suppress_wake {
            tx.execute("INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,'null')", (session_id, WAKE_SUPPRESSION, anchor, ordinal))
                .map_err(|error| write_error(error, "could not record wake suppression".to_owned()))?;
        }
        if let Some(pause) = pause {
            let value = serde_json::to_string(&pause)
                .map_err(|error| write_error(error, "pause cause is not encodable".to_owned()))?;
            tx.execute("INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,?5)", (session_id, PAUSE_CAUSE, anchor, ordinal, value))
                .map_err(|error| write_error(error, "could not record pause cause".to_owned()))?;
        }
        tx.commit()
            .map_err(|error| write_error(error, "could not commit host intents".to_owned()))
    }

    /// Writes a root loss intent at the current applied cursor, after every
    /// host intent already at that cursor.
    pub fn write_root_loss_at_cursor(
        &mut self,
        session_id: &str,
        intent: &RootLossIntent,
    ) -> Result<(), RecordStoreError> {
        let value = intent.encode()?;
        let kind = match intent {
            RootLossIntent::Interrupt(_) => ROOT_INTERRUPT,
            RootLossIntent::Continuation(_) => ROOT_CONTINUATION,
        };
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| write_error(error, "could not start host intent write".to_owned()))?;
        let (anchor, ordinal) = cursor_slot(&tx, session_id)?;
        tx.execute(
            "INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,?5)",
            (session_id, kind, anchor, ordinal, value),
        )
        .map_err(|error| write_error(error, format!("could not record the {kind} intent")))?;
        tx.commit()
            .map_err(|error| write_error(error, "could not commit host intents".to_owned()))
    }
}

/// The applied cursor of an existing session and the next host intent
/// ordinal at it.
fn cursor_slot(tx: &Transaction<'_>, session_id: &str) -> Result<(i64, u32), RecordStoreError> {
    let exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [session_id],
            |row| row.get(0),
        )
        .map_err(|error| write_error(error, "could not inspect host intent session".to_owned()))?;
    if !exists {
        return Err("session not found".to_owned().into());
    }
    let anchor: i64 = tx
        .query_row(
            "SELECT COALESCE((SELECT applied_seq FROM projector_cursors WHERE session_id=?1),0)",
            [session_id],
            |row| row.get(0),
        )
        .map_err(|error| write_error(error, "could not inspect host intent cursor".to_owned()))?;
    let last: Option<u32> = tx
        .query_row(
            "SELECT MAX(ordinal) FROM host_intents WHERE session_id=?1 AND anchor=?2",
            (session_id, anchor),
            |row| row.get(0),
        )
        .map_err(|error| write_error(error, "could not inspect host intent ordinal".to_owned()))?;
    let ordinal = last
        .map_or(Some(0), |ordinal| ordinal.checked_add(1))
        .ok_or_else(|| "host intent ordinal is exhausted".to_owned())?;
    Ok((anchor, ordinal))
}
