use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use plakous_hosted::sessions::projector::{Submission, SubmissionKind};
use rusqlite::{Connection, Params, TransactionState};

use super::{from_i64, invalid, unavailable};

/// Verified immutable fields, by owner and command id. The rowid index only
/// names fields already verified in a committed snapshot on this connection.
#[derive(Debug, Default)]
pub(in crate::store) struct VerifiedSubmissions {
    fields: HashMap<String, HashMap<String, Submission>>,
    index: Option<SubmissionIndex>,
}

#[derive(Debug, Default)]
struct SubmissionIndex {
    changed: Arc<Mutex<BTreeSet<i64>>>,
    version: Option<(i64, i64)>,
    rows: HashMap<i64, (String, String)>,
}

impl SubmissionIndex {
    fn invalidate(&mut self, connection: &Connection) -> Result<(), String> {
        let version = (
            connection
                .query_row("PRAGMA data_version", [], |row| row.get(0))
                .map_err(unavailable)?,
            connection
                .query_row("PRAGMA schema_version", [], |row| row.get(0))
                .map_err(unavailable)?,
        );
        if self.version != Some(version) {
            self.rows.clear();
            self.version = Some(version);
        }
        let changed =
            std::mem::take(&mut *self.changed.lock().unwrap_or_else(PoisonError::into_inner));
        for rowid in changed {
            self.rows.remove(&rowid);
        }
        Ok(())
    }
}

impl VerifiedSubmissions {
    // Owns the product connection's only update hook. The opener skips this
    // when the crash-sweep tracer owns that hook; then no rowid index exists.
    pub(in crate::store) fn track_updates(
        &mut self,
        connection: &mut Connection,
    ) -> rusqlite::Result<()> {
        let index = SubmissionIndex::default();
        let changed = Arc::clone(&index.changed);
        connection.update_hook(Some(move |_, database: &str, table: &str, rowid| {
            if database == "main" && table == "submissions" {
                changed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(rowid);
            }
        }))?;
        self.index = Some(index);
        Ok(())
    }

    fn fields(&self, owner: &str, command: &str) -> Option<&Submission> {
        self.fields.get(owner)?.get(command)
    }

    pub(in crate::store) fn forget(&mut self, owner: &str, command_id: &str) {
        if let Some(verified) = self.fields.get_mut(owner) {
            verified.remove(command_id);
        }
    }

    pub(in crate::store) fn forget_session(&mut self, owner: &str) {
        self.fields.remove(owner);
    }

    fn indexed(&self, rowid: i64) -> Option<(String, Submission)> {
        let (owner, command) = self.index.as_ref()?.rows.get(&rowid)?;
        Some((owner.clone(), self.fields(owner, command)?.clone()))
    }
}

type StoredSubmission = (i64, String, Submission);

fn read_rows(
    connection: &Connection,
    filter: &str,
    params: impl Params,
) -> Result<Vec<StoredSubmission>, String> {
    connection
        .prepare(&format!(
            "SELECT rowid,session_id,command_id,kind,queue_item_id,queue_revision,target_turn_id,canonical_command,submitted_at_ms FROM submissions WHERE {filter} ORDER BY rowid"
        ))
        .map_err(unavailable)?
        .query_map(params, |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                Submission {
                    command_id: row.get(2)?,
                    kind: SubmissionKind::parse(&row.get::<_, String>(3)?)
                        .ok_or(rusqlite::Error::InvalidQuery)?,
                    queue_item_id: row.get(4)?,
                    queue_item_revision: from_i64(row.get(5)?)?,
                    target_turn_id: row.get(6)?,
                    canonical_command: row.get(7)?,
                    submitted_at_ms: from_i64(row.get(8)?)?,
                },
            ))
        })
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)
}

fn verify_rows(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
    rows: &[StoredSubmission],
) -> Result<(), String> {
    let unverified = rows
        .iter()
        .filter(|(_, owner, submission)| {
            verified.fields(owner, &submission.command_id) != Some(submission)
        })
        .collect::<Vec<_>>();
    if !unverified.is_empty() {
        let identities = serde_json::to_string(
            &unverified
                .iter()
                .map(|(_, _, row)| row.binding_identity())
                .collect::<Vec<_>>(),
        )
        .map_err(|_| "submission identities are not encodable".to_owned())?;
        let bindings = connection
            .prepare(
                "SELECT b.identity,b.owner_session_id,b.binding_digest FROM json_each(?1) AS wanted JOIN identity_bindings AS b ON b.identity=wanted.value WHERE b.kind='command' AND b.terminal_disposition IS NULL AND b.terminal_outcome IS NULL",
            )
            .map_err(unavailable)?
            .query_map([identities], |row| {
                Ok((row.get::<_, String>(0)?, (row.get::<_, String>(1)?, row.get::<_, String>(2)?)))
            })
            .map_err(unavailable)?
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(unavailable)?;
        for (_, owner, submission) in &unverified {
            let (bound_owner, digest) =
                bindings
                    .get(&submission.binding_identity())
                    .ok_or_else(|| {
                        "submission contradicts its permanent identity binding".to_owned()
                    })?;
            if bound_owner != owner || *digest != submission.binding_digest(owner) {
                return Err("submission contradicts its permanent identity binding".to_owned());
            }
        }
    }
    // A row read in a write transaction can still be rolled back.
    if matches!(
        connection.transaction_state(None::<&str>),
        Ok(TransactionState::None | TransactionState::Read)
    ) {
        for (_, owner, submission) in unverified {
            verified
                .fields
                .entry(owner.clone())
                .or_default()
                .insert(submission.command_id.clone(), submission.clone());
        }
        if let Some(index) = &mut verified.index {
            for (rowid, owner, submission) in rows {
                index
                    .rows
                    .insert(*rowid, (owner.clone(), submission.command_id.clone()));
            }
        }
    }
    Ok(())
}

fn read_indexed(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
    filter: &str,
    value: &str,
) -> Result<Vec<(String, Submission)>, String> {
    let rowids = connection
        .prepare(&format!(
            "SELECT rowid FROM submissions WHERE {filter} ORDER BY rowid"
        ))
        .map_err(unavailable)?
        .query_map([value], |row| row.get::<_, i64>(0))
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    if let Some(index) = &mut verified.index {
        index.invalidate(connection)?;
    }
    let requested = rowids
        .into_iter()
        .map(|rowid| (rowid, verified.indexed(rowid)))
        .collect::<Vec<_>>();
    let wanted = requested
        .iter()
        .filter_map(|(rowid, cached)| cached.is_none().then_some(rowid))
        .collect::<Vec<_>>();
    let mut loaded = HashMap::new();
    if !wanted.is_empty() {
        let wanted = serde_json::to_string(&wanted)
            .map_err(|_| "submission row keys are not encodable".to_owned())?;
        let rows = read_rows(
            connection,
            "rowid IN (SELECT value FROM json_each(?1))",
            [wanted],
        )?;
        verify_rows(connection, verified, &rows)?;
        loaded.extend(
            rows.into_iter()
                .map(|(rowid, owner, row)| (rowid, (owner, row))),
        );
    }
    requested
        .into_iter()
        .map(|(rowid, cached)| {
            cached
                .or_else(|| loaded.remove(&rowid))
                .ok_or_else(|| "submission changed within its read snapshot".to_owned())
        })
        .collect()
}

pub(super) fn load_submissions_where(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
    filter: &str,
    value: &str,
) -> Result<Vec<(String, Submission)>, String> {
    if verified.index.is_none() {
        let rows = read_rows(connection, filter, [value])?;
        verify_rows(connection, verified, &rows)?;
        return Ok(rows
            .into_iter()
            .map(|(_, owner, row)| (owner, row))
            .collect());
    }

    // The key scan pins the same snapshot as the data/schema versions and
    // any body reads. Never use the index across unrelated autocommit reads.
    if !connection.is_autocommit() {
        return read_indexed(connection, verified, filter, value);
    }
    let tx = connection.unchecked_transaction().map_err(unavailable)?;
    let rows = read_indexed(&tx, verified, filter, value)?;
    tx.commit().map_err(unavailable)?;
    Ok(rows)
}
