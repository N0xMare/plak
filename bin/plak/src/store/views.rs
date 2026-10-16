//! Schema 31 intents and projector views (spec 02 §2.2, §2.6).
//!
//! Intent writes commit under `synchronous=FULL`; a projector transaction
//! commits under `NORMAL` and advances the cursor by CAS from the value its
//! run read, so a concurrent applier's transaction changes nothing. This is
//! the only module that writes the projector view tables.

use std::collections::BTreeMap;

use plakous::sessions::SessionLocator;
use plakous::sessions::feed::FeedSeq;
use plakous_hosted::sessions::projector::{
    AnchoredIntent, HostPauseCause, Lane, OperatorNotice, ProductIntents, ProjectionCommit, RowKey,
    SessionViewState, SessionViews, Submission, SubmissionKind, ViewWrites, WorkflowRequest,
};
use plakous_hosted::sessions::{TranscriptRow, encode_workflow_kind};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use super::{ProductStore, decode_workflow_kind, from_i64, to_i64};

const WAKE_SUPPRESSION: &str = "wake_suppression";
const PAUSE_CAUSE: &str = "pause_cause";
/// Imported rows are notices at anchor 0 under this identity prefix.
const IMPORTED_NOTICE_PREFIX: &str = "plak:import:";

fn invalid(_: rusqlite::Error) -> String {
    "product view row is invalid".to_owned()
}

fn unavailable(_: rusqlite::Error) -> String {
    "product store is unavailable".to_owned()
}

fn projection_refusal(error: rusqlite::Error) -> Result<ProjectionCommit, String> {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) {
        return Ok(ProjectionCommit::CursorMoved);
    }
    Err(unavailable(error))
}

impl ProductStore {
    pub fn load_inherited_submissions(&self, id: &str) -> Result<Vec<Submission>, String> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(unavailable)?;
        let submissions = inherited_submissions_of(&tx, id)?;
        tx.commit().map_err(unavailable)?;
        Ok(submissions)
    }

    pub fn write_host_controls(
        &mut self,
        session_id: &str,
        suppress_wake: bool,
        pause: Option<HostPauseCause>,
    ) -> Result<(), String> {
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "could not start host intent write".to_owned())?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                [session_id],
                |row| row.get(0),
            )
            .map_err(|_| "could not inspect host intent session".to_owned())?;
        if !exists {
            return Err("session not found".to_owned());
        }
        let anchor: i64 = tx.query_row("SELECT COALESCE((SELECT applied_seq FROM projector_cursors WHERE session_id=?1),0)", [session_id], |row| row.get(0))
            .map_err(|_| "could not inspect host intent cursor".to_owned())?;
        let last: Option<u32> = tx
            .query_row(
                "SELECT MAX(ordinal) FROM host_intents WHERE session_id=?1 AND anchor=?2",
                params![session_id, anchor],
                |row| row.get(0),
            )
            .map_err(|_| "could not inspect host intent ordinal".to_owned())?;
        let ordinal = last
            .map_or(Some(0), |ordinal| ordinal.checked_add(1))
            .ok_or_else(|| "host intent ordinal is exhausted".to_owned())?;
        if suppress_wake {
            tx.execute("INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,'null')", params![session_id, WAKE_SUPPRESSION, anchor, ordinal])
                .map_err(|_| "could not record wake suppression".to_owned())?;
        }
        if let Some(pause) = pause {
            let value = serde_json::to_string(&pause)
                .map_err(|_| "pause cause is not encodable".to_owned())?;
            tx.execute("INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,?5)", params![session_id, PAUSE_CAUSE, anchor, ordinal, value])
                .map_err(|_| "could not record pause cause".to_owned())?;
        }
        tx.commit()
            .map_err(|_| "could not commit host intents".to_owned())
    }

    /// The same-build source whose tree a successor inherits. Carry sources
    /// are references to another state directory, never topology authority.
    #[cfg(feature = "maintainer")]
    pub(crate) fn inspect_tree_source_read_only(
        paths: &crate::home::paths::PlakPaths,
        session_id: &str,
    ) -> Result<Option<String>, String> {
        Self::read_only_snapshot(paths, |snapshot| {
            let (source, operation): (Option<String>, Option<String>) = snapshot
                .query_row(
                    "SELECT source_session_id,source_operation_id FROM sessions WHERE id=?1",
                    [session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(unavailable)?;
            Ok(
                if operation.as_deref().is_some_and(|operation| {
                    operation.starts_with(plakous_hosted::sessions::CARRY_FORWARD_OPERATION_PREFIX)
                }) {
                    None
                } else {
                    source
                },
            )
        })
    }

    /// A rehearsal's construction fields, intents and views from one read-only
    /// snapshot. No legacy delivery, workflow or head-adoption state is read.
    #[cfg(feature = "maintainer")]
    pub(crate) fn inspect_projection_read_only(
        paths: &crate::home::paths::PlakPaths,
        session_id: &str,
    ) -> Result<
        (
            std::path::PathBuf,
            Option<SessionLocator>,
            ProductIntents,
            Option<SessionViews>,
        ),
        String,
    > {
        Self::read_only_snapshot(paths, |snapshot| {
            let (workspace, recorded): (String, Option<String>) = snapshot
                .query_row(
                    "SELECT workspace_root,locator FROM sessions WHERE id=?1",
                    [session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(unavailable)?;
            let recorded = recorded
                .map(|locator| locator.parse::<SessionLocator>())
                .transpose()
                .map_err(|_| "invalid construction binding".to_owned())?;
            let runtime = recorded.map(SessionLocator::session_id);
            let mut intents = intents_of(snapshot, session_id)?;
            let mut current = session_id.to_owned();
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if !seen.insert(current.clone()) {
                    return Err("projection intent ancestry is cyclic".to_owned());
                }
                let (source, operation): (Option<String>, Option<String>) = snapshot
                    .query_row(
                        "SELECT source_session_id,source_operation_id FROM sessions WHERE id=?1",
                        [&current],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(unavailable)?;
                if operation.as_deref().is_some_and(|operation| {
                    operation.starts_with(plakous_hosted::sessions::CARRY_FORWARD_OPERATION_PREFIX)
                }) {
                    break;
                }
                let Some(source) = source else { break };
                let locator: Option<String> = snapshot
                    .query_row(
                        "SELECT locator FROM sessions WHERE id=?1",
                        [&source],
                        |row| row.get(0),
                    )
                    .map_err(unavailable)?;
                let source_runtime = locator
                    .map(|locator| locator.parse::<SessionLocator>())
                    .transpose()
                    .map_err(|_| "invalid source construction binding".to_owned())?
                    .map(SessionLocator::session_id);
                if runtime.is_none() || source_runtime != runtime {
                    break;
                }
                intents
                    .submissions
                    .extend(intents_of(snapshot, &source)?.submissions);
                current = source;
            }
            Ok((
                workspace.into(),
                recorded,
                intents,
                views_of(snapshot, session_id)?,
            ))
        })
    }

    /// Writes one command submission before it is submitted. An identical
    /// repetition is a no-op; any other row under its command id is refused.
    pub fn write_submission(
        &mut self,
        session_id: &str,
        submission: &Submission,
    ) -> Result<(), String> {
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        insert_submission(&tx, session_id, submission)?;
        tx.commit().map_err(unavailable)
    }

    /// Writes one workflow request before it is submitted.
    pub fn write_workflow_request(
        &mut self,
        session_id: &str,
        request: &WorkflowRequest,
    ) -> Result<(), String> {
        self.durability(true)?;
        let (reason, selection) = encode_workflow_kind(&request.kind);
        let changed = self
            .connection
            .execute(
                "INSERT INTO workflow_requests(session_id,operation_id,source_locator,kind,compact_reason,rebind_selection,anchor) VALUES(?1,?2,?3,?4,?5,?6,COALESCE((SELECT applied_seq FROM projector_cursors WHERE session_id=?1),0)) ON CONFLICT(operation_id) DO NOTHING",
                params![
                    session_id,
                    request.operation_id,
                    request.source_locator.to_string(),
                    request.kind.as_str(),
                    reason,
                    selection,
                ],
            )
            .map_err(unavailable)?;
        if changed == 0 {
            let existing = self.load_intents(session_id)?;
            if existing.workflow_request(&request.operation_id) != Some(request) {
                return Err(format!(
                    "workflow operation id {} is already bound to another request",
                    request.operation_id
                ));
            }
        }
        Ok(())
    }

    /// Writes a live request under the authoring session's revision CAS.
    pub fn submit_workflow_request(
        &mut self,
        session: &mut plakous_hosted::sessions::SessionRecord,
        request: &WorkflowRequest,
    ) -> Result<(), String> {
        plakous::sessions::SessionOperationId::new(&request.operation_id)
            .map_err(|_| "workflow operation identity is invalid".to_owned())?;
        if session.plakous_locator != Some(request.source_locator) {
            return Err("workflow source is not the exact product binding".to_owned());
        }
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        let (reason, selection) = encode_workflow_kind(&request.kind);
        let inserted = tx.execute(
            "INSERT INTO workflow_requests(session_id,operation_id,source_locator,kind,compact_reason,rebind_selection,anchor) VALUES(?1,?2,?3,?4,?5,?6,COALESCE((SELECT applied_seq FROM projector_cursors WHERE session_id=?1),0)) ON CONFLICT(operation_id) DO NOTHING",
            params![session.id, request.operation_id, request.source_locator.to_string(), request.kind.as_str(), reason, selection],
        ).map_err(unavailable)?;
        if inserted == 0
            && intents_of(&tx, &session.id)?.workflow_request(&request.operation_id)
                != Some(request)
        {
            return Err("workflow identity is bound to another request".to_owned());
        }
        super::bind_identity(
            &tx,
            "workflow",
            &request.operation_id,
            &session.id,
            &request.binding_digest(&session.id),
            None,
            None,
        )?;
        let next = workflow_intent_cas(&tx, session)?;
        if request.kind == plakous_hosted::sessions::SessionWorkflowKind::Clear {
            let target = session.clear_successor_from_request(request)?;
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    [&target.id],
                    |row| row.get(0),
                )
                .map_err(unavailable)?;
            if exists {
                let existing = super::load_record(&tx, &target.id)?;
                if existing.create_operation_id != target.create_operation_id
                    || existing.source_product_session_id != target.source_product_session_id
                    || existing.source_workflow_operation_id != target.source_workflow_operation_id
                {
                    return Err("clear successor identity is bound to another request".to_owned());
                }
            } else {
                super::commit_in_transaction(&tx, &target, 0, None)?;
            }
        }
        tx.commit().map_err(unavailable)?;
        session.store_revision = next;
        session.touch();
        Ok(())
    }

    /// Removes a definitively unadmitted live request under the session CAS.
    pub fn discard_unadmitted_workflow(
        &mut self,
        session: &mut plakous_hosted::sessions::SessionRecord,
        operation_id: &str,
    ) -> Result<(), String> {
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        let views = views_of(&tx, &session.id)?
            .ok_or_else(|| "workflow discard requires projected views".to_owned())?;
        let intents = intents_of(&tx, &session.id)?;
        let Some(request) = intents.workflow_request(operation_id) else {
            return Ok(());
        };
        if !plakous_hosted::sessions::projector::pending_workflow_operation_ids(&views, &intents)
            .iter()
            .any(|id| id == operation_id)
        {
            return Err("a recorded workflow request cannot be discarded".to_owned());
        }
        let next = workflow_intent_cas(&tx, session)?;
        super::bind_identity(
            &tx,
            "workflow",
            operation_id,
            &session.id,
            &request.binding_digest(&session.id),
            Some("discarded"),
            None,
        )?;
        tx.execute(
            "DELETE FROM workflow_requests WHERE session_id=?1 AND operation_id=?2",
            params![session.id, operation_id],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        session.store_revision = next;
        session.touch();
        Ok(())
    }

    /// Withdraws unadmitted requests and their notices in one FULL commit.
    pub fn withdraw_unadmitted_workflows(
        &mut self,
        session: &mut plakous_hosted::sessions::SessionRecord,
    ) -> Result<Vec<TranscriptRow>, String> {
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        let views = views_of(&tx, &session.id)?
            .ok_or_else(|| "workflow withdrawal requires projected views".to_owned())?;
        let intents = intents_of(&tx, &session.id)?;
        let withdrawals =
            plakous_hosted::sessions::projector::workflow_withdrawals(&views, &intents);
        if withdrawals.is_empty() {
            return Ok(Vec::new());
        }
        let next = workflow_intent_cas(&tx, session)?;
        let mut rows = Vec::new();
        for (id, notice) in withdrawals {
            if let Some(request) = intents.workflow_request(&id)
                && request.kind == plakous_hosted::sessions::SessionWorkflowKind::Clear
            {
                let target_id = request.successor_product_id();
                let target = match super::load_record(&tx, &target_id) {
                    Ok(target) => Some(target),
                    Err(error) if error == "session not found" => None,
                    Err(error) => return Err(error),
                };
                if let Some(target) = target {
                    if views_of(&tx, &target_id)?.is_some()
                        || target.source_product_session_id.as_deref() != Some(session.id.as_str())
                        || target.source_workflow_operation_id.as_deref() != Some(id.as_str())
                        || !target.prompt_history.is_empty()
                        || !target.pending_queue.is_empty()
                        || intents_of(&tx, &target_id)? != ProductIntents::default()
                    {
                        return Err(
                            "unadmitted clear successor is not an empty reservation".to_owned()
                        );
                    }
                    // The successful source pass proved this reserved genesis has
                    // no runtime session. Identity bindings deliberately survive.
                    tx.execute("DELETE FROM sessions WHERE id=?1", [&target_id])
                        .map_err(unavailable)?;
                }
            }
            tx.execute(
                "DELETE FROM workflow_requests WHERE session_id=?1 AND operation_id=?2",
                params![session.id, id],
            )
            .map_err(unavailable)?;
            insert_notice(&tx, &session.id, &notice)?;
            rows.push(notice.row);
        }
        tx.commit().map_err(unavailable)?;
        session.store_revision = next;
        session.touch();
        Ok(rows)
    }

    /// Writes a child-wake suppression intent.
    pub fn write_wake_suppression(
        &mut self,
        session_id: &str,
        intent: &AnchoredIntent<()>,
    ) -> Result<(), String> {
        self.write_host_intent(
            session_id,
            WAKE_SUPPRESSION,
            intent.anchor,
            intent.ordinal,
            "null".to_owned(),
        )
    }

    /// Writes a pause cause no fact carries.
    pub fn write_pause_cause(
        &mut self,
        session_id: &str,
        intent: &AnchoredIntent<HostPauseCause>,
    ) -> Result<(), String> {
        let value = serde_json::to_string(&intent.value)
            .map_err(|_| "pause cause is not encodable".to_owned())?;
        self.write_host_intent(
            session_id,
            PAUSE_CAUSE,
            intent.anchor,
            intent.ordinal,
            value,
        )
    }

    fn write_host_intent(
        &mut self,
        session_id: &str,
        kind: &str,
        anchor: u64,
        ordinal: u32,
        value: String,
    ) -> Result<(), String> {
        self.durability(true)?;
        self.connection
            .execute(
                "INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,?5)",
                params![session_id, kind, to_i64(anchor)?, ordinal, value],
            )
            .map_err(|_| format!("could not record the {kind} intent"))?;
        Ok(())
    }

    /// Every intent of one session the projector reads.
    pub fn load_intents(&mut self, session_id: &str) -> Result<ProductIntents, String> {
        intents_of(&self.connection, session_id)
    }

    /// The stored views of one session, or `None` before its first projector
    /// transaction (or after the views were deleted for a rebuild).
    pub fn load_views(&mut self, session_id: &str) -> Result<Option<SessionViews>, String> {
        views_of(&self.connection, session_id)
    }

    /// Commits one projector run: CAS-advances the cursor from the value the
    /// run read and writes the changed rows and the view state in the same
    /// transaction, under `synchronous=NORMAL`.
    pub fn commit_projection_views(
        &mut self,
        session_id: &str,
        writes: &ViewWrites,
        views: &SessionViews,
    ) -> Result<ProjectionCommit, String> {
        self.durability(false)?;
        let tx = match self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
        {
            Ok(tx) => tx,
            Err(error) => return projection_refusal(error),
        };
        // A target join changes source state without advancing its cursor.
        // Recompute it here so a racing source page cannot erase it.
        let mut joined = views.clone();
        let intents = intents_of(&tx, session_id)?;
        for request in &intents.workflow_requests {
            if request.kind != plakous_hosted::sessions::SessionWorkflowKind::Clear
                || intents.workflow_consumed(&request.operation_id, views.state.retained_from)
                || views
                    .state
                    .workflows
                    .get(&request.operation_id)
                    .is_some_and(|workflow| workflow.settled.is_some())
            {
                continue;
            }
            let target_id = request.successor_product_id();
            if let Some(target_views) = views_of(&tx, &target_id)? {
                let target = super::load_record(&tx, &target_id)?;
                if let Some(completion) = plakous_hosted::sessions::projector::clear_completion(
                    request,
                    &target,
                    &target_views,
                ) {
                    joined
                        .state
                        .workflows
                        .insert(request.operation_id.clone(), completion);
                }
            }
        }
        joined.consume_before_retention(&intents);
        let views = &joined;
        if commit_views(&tx, session_id, writes, views)? == ProjectionCommit::CursorMoved {
            return Ok(ProjectionCommit::CursorMoved);
        }
        if views
            .state
            .created_operation_id
            .as_deref()
            .is_some_and(|operation| operation.ends_with(":create"))
        {
            let target = super::load_record(&tx, session_id)?;
            if let (Some(source_id), Some(operation)) = (
                target.source_product_session_id.as_deref(),
                target.source_workflow_operation_id.as_deref(),
            ) {
                let intents = intents_of(&tx, source_id)?;
                if let Some(completion) = intents.workflow_request(operation).and_then(|request| {
                    plakous_hosted::sessions::projector::clear_completion(request, &target, views)
                }) {
                    let previous = views_of(&tx, source_id)?.unwrap_or_else(SessionViews::empty);
                    let mut source_views = previous.clone();
                    source_views
                        .state
                        .workflows
                        .insert(operation.to_owned(), completion);
                    source_views.consume_before_retention(&intents);
                    let writes = ViewWrites::between(&previous, &source_views);
                    commit_views(&tx, source_id, &writes, &source_views)?;
                }
            }
        }
        if !writes.successors.is_empty() {
            let source = super::load_record(&tx, session_id)?;
            let intents = intents_of(&tx, session_id)?;
            for successor in &writes.successors {
                let Some(request) = successor
                    .operation_id
                    .as_deref()
                    .and_then(|id| intents.workflow_request(id))
                else {
                    continue;
                };
                let mut target = source.successor_from_request(request, successor)?;
                let exists: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                        [&target.id],
                        |row| row.get(0),
                    )
                    .map_err(unavailable)?;
                if exists {
                    let existing = super::load_record(&tx, &target.id)?;
                    if existing.create_operation_id != target.create_operation_id
                        || existing.source_product_session_id != target.source_product_session_id
                        || existing.source_workflow_operation_id
                            != target.source_workflow_operation_id
                    {
                        return Err("successor identity is bound to another request".to_owned());
                    }
                    continue;
                }
                let claimed: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM identity_bindings WHERE identity=?1 AND kind='create')",
                        [&target.create_operation_id],
                        |row| row.get(0),
                    )
                    .map_err(unavailable)?;
                if claimed {
                    continue;
                }
                target.store_revision = super::commit_in_transaction(&tx, &target, 0, None)?;
                for notice in &intents.notices {
                    if notice.anchor < successor.published_seq.get() {
                        insert_notice(&tx, &target.id, notice)?;
                    }
                }
                write_import_intents(&tx, &target.id, &intents.imported)?;
            }
        }
        match tx.commit() {
            Ok(()) => Ok(ProjectionCommit::Applied),
            Err(error) => projection_refusal(error),
        }
    }

    /// Deletes one session's views and cursor: always safe, the next
    /// projector run rebuilds them from the feed.
    pub fn delete_views(&mut self, session_id: &str) -> Result<(), String> {
        self.durability(false)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        for table in [
            "transcript_rows",
            "session_call_views",
            "session_open_turn_views",
            "session_process_views",
            "session_views",
            "projector_cursors",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE session_id=?1"),
                [session_id],
            )
            .map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)
    }
}

fn commit_views(
    tx: &Connection,
    session_id: &str,
    writes: &ViewWrites,
    views: &SessionViews,
) -> Result<ProjectionCommit, String> {
    let runtime_session = views
        .state
        .runtime_session_id
        .map(|session| session.to_string())
        .unwrap_or_default();
    let expected = to_i64(writes.expected_cursor.get())?;
    let next = to_i64(writes.cursor.get())?;
    let moved = if writes.expected_cursor == FeedSeq::ZERO {
            tx.execute(
                "INSERT INTO projector_cursors(session_id,runtime_session_id,applied_seq) VALUES(?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET applied_seq=excluded.applied_seq,runtime_session_id=excluded.runtime_session_id WHERE applied_seq=0",
                params![session_id, runtime_session, next],
            )
        } else {
            tx.execute(
                "UPDATE projector_cursors SET applied_seq=?3,runtime_session_id=?2 WHERE session_id=?1 AND applied_seq=?4",
                params![session_id, runtime_session, next, expected],
            )
        }
        .map_err(unavailable)?;
    if moved != 1 {
        return Ok(ProjectionCommit::CursorMoved);
    }
    let state = serde_json::to_string(&views.state)
        .map_err(|_| "product session views are not encodable".to_owned())?;
    let counters = views.state.counters;
    tx.execute(
            "INSERT INTO session_views(session_id,head_locator,root_turn_count,tool_call_count,compaction_count,state) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session_id) DO UPDATE SET head_locator=excluded.head_locator,root_turn_count=excluded.root_turn_count,tool_call_count=excluded.tool_call_count,compaction_count=excluded.compaction_count,state=excluded.state",
            params![
                session_id,
                views.state.head.map(|head| {
                    SessionLocator::new(head.session_id, head.branch_id, head.checkpoint_id)
                        .to_string()
                }),
                to_i64(counters.root_turns)?,
                to_i64(counters.tool_calls)?,
                to_i64(counters.compactions)?,
                state,
            ],
        )
        .map_err(unavailable)?;
    write_keyed(
        tx,
        session_id,
        "session_call_views",
        "call_id",
        &writes.calls,
    )?;
    write_keyed(
        tx,
        session_id,
        "session_open_turn_views",
        "turn_id",
        &writes.open_turns,
    )?;
    let processes = writes
        .processes
        .iter()
        .map(|(key, value)| Ok((to_i64(*key)?, value.clone())))
        .collect::<Result<Vec<_>, String>>()?;
    write_keyed(
        tx,
        session_id,
        "session_process_views",
        "process_id",
        &processes,
    )?;
    for key in &writes.deletes {
        tx.execute(
                "DELETE FROM transcript_rows WHERE session_id=?1 AND anchor=?2 AND lane=?3 AND ordinal=?4",
                params![session_id, to_i64(key.anchor)?, key.lane.number(), key.ordinal],
            )
            .map_err(unavailable)?;
    }
    for (key, row) in &writes.upserts {
        let text =
            serde_json::to_string(row).map_err(|_| "transcript row is not encodable".to_owned())?;
        tx.execute(
                "INSERT INTO transcript_rows(session_id,anchor,lane,ordinal,row,call_id) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session_id,anchor,lane,ordinal) DO UPDATE SET row=excluded.row,call_id=excluded.call_id",
                params![
                    session_id,
                    to_i64(key.anchor)?,
                    key.lane.number(),
                    key.ordinal,
                    text,
                    row.call_id(),
                ],
            )
            .map_err(|_| "could not write a transcript row".to_owned())?;
    }
    Ok(ProjectionCommit::Applied)
}

fn write_keyed<K: rusqlite::ToSql, V: serde::Serialize>(
    connection: &Connection,
    session_id: &str,
    table: &str,
    key_column: &str,
    changes: &[(K, Option<V>)],
) -> Result<(), String> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut upsert = connection.prepare(&format!(
        "INSERT INTO {table}(session_id,{key_column},value) VALUES(?1,?2,?3) ON CONFLICT(session_id,{key_column}) DO UPDATE SET value=excluded.value"
    )).map_err(unavailable)?;
    let mut delete = connection
        .prepare(&format!(
            "DELETE FROM {table} WHERE session_id=?1 AND {key_column}=?2"
        ))
        .map_err(unavailable)?;
    for (key, value) in changes {
        if let Some(value) = value {
            let value = serde_json::to_string(value)
                .map_err(|_| "product keyed view is not encodable".to_owned())?;
            upsert
                .execute(params![session_id, key, value])
                .map_err(unavailable)?;
        } else {
            delete
                .execute(params![session_id, key])
                .map_err(unavailable)?;
        }
    }
    Ok(())
}

fn keyed_of<K: rusqlite::types::FromSql + Ord, V: serde::de::DeserializeOwned>(
    connection: &Connection,
    session_id: &str,
    table: &str,
    key_column: &str,
) -> Result<BTreeMap<K, V>, String> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {key_column},value FROM {table} WHERE session_id=?1"
        ))
        .map_err(unavailable)?;
    statement
        .query_map([session_id], |row| {
            let value: String = row.get(1)?;
            Ok((
                row.get(0)?,
                serde_json::from_str(&value).map_err(|_| rusqlite::Error::InvalidQuery)?,
            ))
        })
        .map_err(unavailable)?
        .collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(invalid)
}

/// Writes one submission inside `tx`: an identical repetition is a no-op, any
/// other row under its command id is refused.
pub(super) fn insert_submission(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    submission: &Submission,
) -> Result<(), String> {
    super::bind_identity(
        tx,
        "command",
        &submission.binding_identity(),
        session_id,
        &submission.binding_digest(session_id),
        None,
        None,
    )?;
    tx.execute(
        "INSERT INTO submissions(session_id,command_id,kind,queue_item_id,queue_revision,target_turn_id,canonical_command,submitted_at_ms,anchor) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,COALESCE((SELECT applied_seq FROM projector_cursors WHERE session_id=?1),0)) ON CONFLICT(command_id) DO NOTHING",
        params![
            session_id,
            submission.command_id,
            submission.kind.as_str(),
            submission.queue_item_id,
            to_i64(submission.queue_item_revision)?,
            submission.target_turn_id,
            submission.canonical_command,
            to_i64(submission.submitted_at_ms)?,
        ],
    )
    .map_err(unavailable)?;
    let stored = load_submissions_where(tx, "command_id=?1", &submission.command_id)?;
    if stored.first().map(|(owner, row)| (owner.as_str(), row)) != Some((session_id, submission)) {
        return Err(format!(
            "command id {} is already bound to another submission",
            submission.command_id
        ));
    }
    Ok(())
}

/// Records the host pause atomically with withdrawal of an unrecorded command.
pub(super) fn insert_pause_cause(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    cause: &HostPauseCause,
) -> Result<(), String> {
    let anchor: i64 = tx
        .query_row(
            "SELECT applied_seq FROM projector_cursors WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(unavailable)?
        .unwrap_or(0);
    let ordinal: u32 = tx.query_row("SELECT COALESCE(MAX(ordinal)+1,0) FROM host_intents WHERE session_id=?1 AND kind=?2 AND anchor=?3", params![session_id, PAUSE_CAUSE, anchor], |row| row.get(0)).map_err(unavailable)?;
    let value =
        serde_json::to_string(cause).map_err(|_| "pause cause is not encodable".to_owned())?;
    tx.execute(
        "INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,?5)",
        params![session_id, PAUSE_CAUSE, anchor, ordinal, value],
    )
    .map_err(unavailable)?;
    Ok(())
}

/// The queue command join needs no transcript or per-call bookkeeping reads.
pub(super) fn queue_commands_of(
    connection: &Connection,
    session_id: &str,
) -> Result<plakous_hosted::sessions::QueueCommands, String> {
    let intents = ProductIntents {
        submission_anchors: intent_anchors(connection, session_id, "submissions", "command_id")?,
        submissions: load_submissions_where(connection, "session_id=?1", session_id)?
            .into_iter()
            .map(|(_, submission)| submission)
            .collect(),
        ..ProductIntents::default()
    };
    let state: Option<String> = connection
        .query_row(
            "SELECT state FROM session_views WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(unavailable)?;
    let mut views = SessionViews::empty();
    if let Some(state) = state {
        views.state = serde_json::from_str(&state)
            .map_err(|_| "product session views are invalid".to_owned())?;
    }
    Ok(plakous_hosted::sessions::QueueCommands::from_join(
        &intents,
        Some(&views),
    ))
}

fn intent_anchors(
    connection: &Connection,
    session_id: &str,
    table: &str,
    identity: &str,
) -> Result<BTreeMap<String, u64>, String> {
    connection
        .prepare(&format!(
            "SELECT {identity},anchor FROM {table} WHERE session_id=?1"
        ))
        .map_err(unavailable)?
        .query_map([session_id], |row| {
            Ok((row.get(0)?, from_i64(row.get(1)?)?))
        })
        .map_err(unavailable)?
        .collect::<Result<_, _>>()
        .map_err(invalid)
}

/// Every intent of one session the projector reads.
fn inherited_submissions_of(connection: &Connection, id: &str) -> Result<Vec<Submission>, String> {
    let metadata = |id: &str| -> Result<_, String> {
        connection
            .query_row(
                "SELECT locator,source_session_id,source_operation_id FROM sessions WHERE id=?1",
                [id],
                |row| {
                    let locator: Option<String> = row.get(0)?;
                    Ok((
                        locator
                            .map(|value| {
                                value
                                    .parse::<SessionLocator>()
                                    .map_err(|_| rusqlite::Error::InvalidQuery)
                            })
                            .transpose()?
                            .map(SessionLocator::session_id),
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .map_err(|error| {
                if error == rusqlite::Error::QueryReturnedNoRows {
                    "session not found".to_owned()
                } else {
                    invalid(error)
                }
            })
    };
    let (runtime, mut source, mut operation) = metadata(id)?;
    let mut seen = std::collections::BTreeSet::from([id.to_owned()]);
    let mut submissions = Vec::new();
    while !operation.as_deref().is_some_and(|operation| {
        operation.starts_with(plakous_hosted::sessions::CARRY_FORWARD_OPERATION_PREFIX)
    }) {
        let Some(source_id) = source else { break };
        if !seen.insert(source_id.clone()) {
            return Err("projection intent ancestry is cyclic".to_owned());
        }
        let (ancestor_runtime, next_source, next_operation) = metadata(&source_id)?;
        if runtime.is_none() || ancestor_runtime != runtime {
            break;
        }
        submissions.extend(
            load_submissions_where(connection, "session_id=?1", &source_id)?
                .into_iter()
                .map(|(_, submission)| submission),
        );
        source = next_source;
        operation = next_operation;
    }
    Ok(submissions)
}

pub(super) fn intents_of(
    connection: &Connection,
    session_id: &str,
) -> Result<ProductIntents, String> {
    let submissions = load_submissions_where(connection, "session_id=?1", session_id)?
        .into_iter()
        .map(|(_, submission)| submission)
        .collect();
    let mut statement = connection
        .prepare("SELECT operation_id,source_locator,kind,compact_reason,rebind_selection FROM workflow_requests WHERE session_id=?1 ORDER BY rowid")
        .map_err(unavailable)?;
    let workflow_requests = statement
        .query_map([session_id], |row| {
            let locator: String = row.get(1)?;
            Ok(WorkflowRequest {
                operation_id: row.get(0)?,
                source_locator: locator
                    .parse::<SessionLocator>()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                kind: decode_workflow_kind(&row.get::<_, String>(2)?, row.get(3)?, row.get(4)?)?,
            })
        })
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    drop(statement);
    let mut statement = connection
        .prepare("SELECT notice_id,anchor,ordinal,row FROM operator_notices WHERE session_id=?1 ORDER BY anchor,ordinal")
        .map_err(unavailable)?;
    let notices = statement
        .query_map([session_id], |row| {
            let text: String = row.get(3)?;
            Ok(OperatorNotice {
                notice_id: row.get(0)?,
                anchor: from_i64(row.get(1)?)?,
                ordinal: row.get(2)?,
                row: serde_json::from_str(&text).map_err(|_| rusqlite::Error::InvalidQuery)?,
            })
        })
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    drop(statement);
    let mut statement = connection
        .prepare("SELECT kind,anchor,ordinal,value FROM host_intents WHERE session_id=?1 ORDER BY anchor,ordinal")
        .map_err(unavailable)?;
    let mut intents = ProductIntents {
        submission_anchors: intent_anchors(connection, session_id, "submissions", "command_id")?,
        workflow_anchors: intent_anchors(
            connection,
            session_id,
            "workflow_requests",
            "operation_id",
        )?,
        submissions,
        workflow_requests,
        notices,
        ..ProductIntents::default()
    };
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                from_i64(row.get(1)?)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    for (kind, anchor, ordinal, value) in rows {
        match kind.as_str() {
            WAKE_SUPPRESSION => intents.wake_suppressions.push(AnchoredIntent {
                anchor,
                ordinal,
                value: (),
            }),
            PAUSE_CAUSE => intents.pause_causes.push(AnchoredIntent {
                anchor,
                ordinal,
                value: serde_json::from_str(&value)
                    .map_err(|_| "pause cause intent is invalid".to_owned())?,
            }),
            _ => return Err("host intent is invalid".to_owned()),
        }
    }
    Ok(intents)
}

/// The stored views of one session, or `None` before its first projector
/// transaction (or after the views were deleted for a rebuild).
pub(super) fn views_of(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<SessionViews>, String> {
    let cursor: Option<i64> = connection
        .query_row(
            "SELECT applied_seq FROM projector_cursors WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(unavailable)?;
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let state: String = connection
        .query_row(
            "SELECT state FROM session_views WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .map_err(|_| "product session views are missing their state".to_owned())?;
    let mut state: SessionViewState =
        serde_json::from_str(&state).map_err(|_| "product session views are invalid".to_owned())?;
    state.calls = keyed_of(connection, session_id, "session_call_views", "call_id")?;
    state.open_turns = keyed_of(connection, session_id, "session_open_turn_views", "turn_id")?;
    let processes: BTreeMap<i64, _> = keyed_of(
        connection,
        session_id,
        "session_process_views",
        "process_id",
    )?;
    state.processes = processes
        .into_iter()
        .map(|(key, value)| Ok((from_i64(key).map_err(invalid)?, value)))
        .collect::<Result<_, String>>()?;
    let mut statement = connection
        .prepare("SELECT anchor,lane,ordinal,row FROM transcript_rows WHERE session_id=?1")
        .map_err(unavailable)?;
    let rows = statement
        .query_map([session_id], |row| {
            let text: String = row.get(3)?;
            Ok((
                RowKey::new(
                    from_i64(row.get(0)?)?,
                    Lane::from_number(row.get(1)?).ok_or(rusqlite::Error::InvalidQuery)?,
                    row.get(2)?,
                ),
                serde_json::from_str::<TranscriptRow>(&text)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
            ))
        })
        .map_err(unavailable)?
        .collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(invalid)?;
    Ok(Some(SessionViews {
        cursor: FeedSeq::new(from_i64(cursor).map_err(invalid)?),
        state,
        rows,
    }))
}

/// Writes a committing record's operator notices as intents, anchored at
/// the cursor the store has applied for it, in the record's transaction. A
/// carried-forward session's first commit writes its imported transcript at
/// anchor 0, lane 1, in document order.
pub(super) fn write_notices(
    connection: &Connection,
    session: &plakous_hosted::sessions::SessionRecord,
    first_commit: bool,
) -> Result<(), String> {
    if first_commit && session.carried_forward_from().is_some() {
        write_import_intents(connection, &session.id, &session.transcript)?;
        return Ok(());
    }
    if session.transcript.pending_notices().is_empty() {
        return Ok(());
    }
    let anchor: i64 = connection
        .query_row(
            "SELECT applied_seq FROM projector_cursors WHERE session_id=?1",
            [&session.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(unavailable)?
        .unwrap_or(0);
    let mut next: u32 = connection
        .query_row(
            "SELECT COALESCE(MAX(ordinal)+1,0) FROM operator_notices WHERE session_id=?1 AND anchor=?2",
            params![session.id, anchor],
            |row| row.get(0),
        )
        .map_err(unavailable)?;
    for row in session.transcript.pending_notices() {
        insert_notice(
            connection,
            &session.id,
            &OperatorNotice {
                notice_id: uuid::Uuid::new_v4().to_string(),
                anchor: from_i64(anchor).map_err(invalid)?,
                ordinal: next,
                row: row.clone(),
            },
        )?;
        next = next.saturating_add(1);
    }
    Ok(())
}

/// The persisted transcript of one session: its views' rows merged with its
/// notices and imported rows (spec 02 §2.6), and the views themselves.
pub(super) fn transcript_of(
    connection: &Connection,
    session_id: &str,
) -> Result<
    (
        plakous_hosted::sessions::ProductTranscript,
        Option<SessionViews>,
        plakous_hosted::sessions::QueueCommands,
    ),
    String,
> {
    let intents = intents_of(connection, session_id)?;
    let views = views_of(connection, session_id)?;
    let queue_commands =
        plakous_hosted::sessions::QueueCommands::from_join(&intents, views.as_ref());
    let rows = views
        .as_ref()
        .unwrap_or(&SessionViews::empty())
        .transcript(&intents)
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    Ok((
        plakous_hosted::sessions::ProductTranscript::committed(rows),
        views,
        queue_commands,
    ))
}

pub(super) fn write_import_intents(
    connection: &Connection,
    session_id: &str,
    rows: &[TranscriptRow],
) -> Result<(), String> {
    for (ordinal, row) in rows.iter().enumerate() {
        insert_notice(
            connection,
            session_id,
            &OperatorNotice {
                notice_id: format!("{IMPORTED_NOTICE_PREFIX}{ordinal}"),
                anchor: 0,
                ordinal: u32::try_from(ordinal)
                    .map_err(|_| "imported transcript is too long".to_owned())?,
                row: row.clone(),
            },
        )?;
    }
    Ok(())
}

fn workflow_intent_cas(
    tx: &Connection,
    session: &plakous_hosted::sessions::SessionRecord,
) -> Result<u64, String> {
    let next = session
        .store_revision
        .checked_add(1)
        .ok_or_else(|| "product session revision is exhausted".to_owned())?;
    let changed = tx
        .execute(
            "UPDATE sessions SET revision=?3,updated_at=?4 WHERE id=?1 AND revision=?2",
            params![
                session.id,
                to_i64(session.store_revision)?,
                to_i64(next)?,
                to_i64(plakous::model::now_unix_secs())?
            ],
        )
        .map_err(unavailable)?;
    if changed != 1 {
        return Err("product session changed concurrently".to_owned());
    }
    Ok(next)
}

fn insert_notice(
    connection: &Connection,
    session_id: &str,
    notice: &OperatorNotice,
) -> Result<(), String> {
    let row = serde_json::to_string(&notice.row)
        .map_err(|_| "operator notice is not encodable".to_owned())?;
    connection
        .execute(
            "INSERT INTO operator_notices(session_id,notice_id,anchor,ordinal,row) VALUES(?1,?2,?3,?4,?5)",
            params![session_id, notice.notice_id, to_i64(notice.anchor)?, notice.ordinal, row],
        )
        .map_err(|_| format!("could not record operator notice {}", notice.notice_id))?;
    Ok(())
}

fn load_submissions_where(
    connection: &Connection,
    filter: &str,
    value: &str,
) -> Result<Vec<(String, Submission)>, String> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT session_id,command_id,kind,queue_item_id,queue_revision,target_turn_id,canonical_command,submitted_at_ms FROM submissions WHERE {filter} ORDER BY rowid"
        ))
        .map_err(unavailable)?;
    let submissions = statement
        .query_map([value], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Submission {
                    command_id: row.get(1)?,
                    kind: SubmissionKind::parse(&row.get::<_, String>(2)?)
                        .ok_or(rusqlite::Error::InvalidQuery)?,
                    queue_item_id: row.get(3)?,
                    queue_item_revision: from_i64(row.get(4)?)?,
                    target_turn_id: row.get(5)?,
                    canonical_command: row.get(6)?,
                    submitted_at_ms: from_i64(row.get(7)?)?,
                },
            ))
        })
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    for (owner, submission) in &submissions {
        let matches: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM identity_bindings WHERE identity=?1 AND kind='command' AND owner_session_id=?2 AND binding_digest=?3 AND terminal_disposition IS NULL AND terminal_outcome IS NULL)",
                params![submission.binding_identity(), owner, submission.binding_digest(owner)],
                |row| row.get(0),
            )
            .map_err(unavailable)?;
        if !matches {
            return Err("submission contradicts its permanent identity binding".to_owned());
        }
    }
    Ok(submissions)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use std::path::{Path, PathBuf};

    use plakous_hosted::sessions::projector::{
        CallView, Counters, OpenTurn, ProcessTrack, SessionViewState,
    };
    use plakous_hosted::sessions::{
        NoticeScope, ProductCompactReason, SessionRecord, SessionWorkflowKind,
    };
    use tempfile::tempdir;

    use super::*;
    use crate::home::paths::PlakPaths;

    fn uuid(n: u64) -> String {
        format!("00000000-0000-4000-8000-{n:012x}")
    }

    fn store_with_session(root: &Path) -> (ProductStore, String) {
        let mut store = ProductStore::open(&PlakPaths::explicit(root.join("p"))).unwrap();
        let mut session = SessionRecord::new(PathBuf::from("/work"));
        store.commit(&mut session).unwrap();
        (store, session.id)
    }

    #[test]
    fn inherited_submissions_do_not_hydrate_ancestor_transcripts_or_other_intents() {
        let root = tempdir().unwrap();
        let (mut store, id) = store_with_session(root.path());
        let mut source = store.load(&id).unwrap();
        let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
        source.bind_plakous_locator(locator).unwrap();
        store.commit(&mut source).unwrap();
        let mut child = SessionRecord::new(source.workspace_root.clone());
        child.bind_plakous_locator(locator).unwrap();
        child.source_product_session_id = Some(id.clone());
        child.source_workflow_operation_id = Some("plak:fork:narrow-read".into());
        store.commit(&mut child).unwrap();
        let views = views_at(1, &[]);
        store
            .commit_projection_views(
                &id,
                &ViewWrites::between(&SessionViews::empty(), &views),
                &views,
            )
            .unwrap();
        // Valid SQLite rows with invalid presentation/workflow encodings would
        // fail a full record load. An ancestry read does not need either.
        store.connection.execute("INSERT INTO transcript_rows(session_id,anchor,lane,ordinal,row) VALUES(?1,1,0,0,'{\"kind\":\"invalid\"}')", [&id]).unwrap();
        assert!(store.load(&id).is_err());
        assert_eq!(
            store.load_inherited_submissions(&child.id).unwrap(),
            [] as [Submission; 0]
        );
        store
            .connection
            .execute("DELETE FROM transcript_rows WHERE session_id=?1", [&id])
            .unwrap();
        store.connection.execute("INSERT INTO workflow_requests(session_id,operation_id,source_locator,kind,anchor) VALUES(?1,'unrelated','invalid','fork',0)", [&id]).unwrap();
        assert!(store.load(&id).is_err());
        assert_eq!(
            store.load_inherited_submissions(&child.id).unwrap(),
            [] as [Submission; 0]
        );
    }

    #[test]
    fn a_cross_connection_writer_moves_the_projection_cursor_without_store_failure() {
        let root = tempdir().unwrap();
        let (mut store, id) = store_with_session(root.path());
        let paths = PlakPaths::explicit(root.path().join("p"));
        let mut other = ProductStore::open(&paths).unwrap();
        store
            .connection
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        let tx = other
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let views = views_at(1, &[(1, TranscriptRow::user("once"))]);
        let writes = ViewWrites::between(&SessionViews::empty(), &views);
        assert_eq!(
            store.commit_projection_views(&id, &writes, &views).unwrap(),
            ProjectionCommit::CursorMoved
        );
        assert!(store.load_views(&id).unwrap().is_none());
        tx.rollback().unwrap();
        assert_eq!(
            other.commit_projection_views(&id, &writes, &views).unwrap(),
            ProjectionCommit::Applied
        );
        assert_eq!(
            store.commit_projection_views(&id, &writes, &views).unwrap(),
            ProjectionCommit::CursorMoved
        );
        assert_eq!(store.load_views(&id).unwrap(), Some(views));
        store.delete_views(&id).unwrap();
        assert!(other.load_views(&id).unwrap().is_none());
    }

    #[test]
    fn historical_clear_and_submissions_are_consumed_without_withdrawal_after_retention() {
        assert_eq!(crate::store::SCHEMA_VERSION, 32);
        let root = tempdir().unwrap();
        let (mut store, id) = store_with_session(root.path());
        let mut source = store.load(&id).unwrap();
        let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
        source.bind_plakous_locator(locator).unwrap();
        store.commit(&mut source).unwrap();
        let request = WorkflowRequest {
            operation_id: "plak:clear:retained".into(),
            source_locator: locator,
            kind: SessionWorkflowKind::Clear,
        };
        store
            .submit_workflow_request(&mut source, &request)
            .unwrap();
        let submission = Submission {
            command_id: "old-root".into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: "old-item".into(),
            queue_item_revision: 1,
            target_turn_id: None,
            canonical_command: "exact command".into(),
            submitted_at_ms: 42,
        };
        store.write_submission(&id, &submission).unwrap();
        // The target's creation fact is gone too. Its nonempty views prove
        // this is a historical clear, not an unadmitted empty reservation.
        let target = views_at(8, &[(8, TranscriptRow::user("target history"))]);
        store
            .commit_projection_views(
                &request.successor_product_id(),
                &ViewWrites::between(&SessionViews::empty(), &target),
                &target,
            )
            .unwrap();
        for deleted in [false, true] {
            if deleted {
                store.delete_views(&id).unwrap();
            }
            let previous = store
                .load_views(&id)
                .unwrap()
                .unwrap_or_else(SessionViews::empty);
            let mut retained = views_at(10, &[(10, TranscriptRow::user("head history"))]);
            retained.state.retained_from = Some(9);
            store
                .commit_projection_views(&id, &ViewWrites::between(&previous, &retained), &retained)
                .unwrap();
            let views = store.load_views(&id).unwrap().unwrap();
            store.write_submission(&id, &submission).unwrap();
            store.write_workflow_request(&id, &request).unwrap();
            let intents = store.load_intents(&id).unwrap();
            assert_eq!(intents.submission_anchors[&submission.command_id], 0);
            assert_eq!(intents.workflow_anchors[&request.operation_id], 0);
            assert_eq!(
                plakous_hosted::sessions::projector::pending_command_ids(&views, &intents),
                [] as [String; 0]
            );
            assert_eq!(
                plakous_hosted::sessions::projector::pending_workflow_operation_ids(
                    &views, &intents
                ),
                [] as [String; 0]
            );
            assert_eq!(
                store.withdraw_unadmitted_workflows(&mut source).unwrap(),
                [] as [TranscriptRow; 0]
            );
            assert_eq!(
                store.load_intents(&id).unwrap().notices,
                [] as [OperatorNotice; 0]
            );
            assert!(store.load(&request.successor_product_id()).is_ok());
            assert!(store.load(&id).unwrap().queue_commands.is_consumed(
                &plakous_hosted::sessions::QueueItem {
                    id: submission.queue_item_id.clone(),
                    revision: 1,
                    text: "old item".into(),
                    submitted_at_ms: 42,
                }
            ));
        }
        let newer = WorkflowRequest {
            operation_id: "plak:compact:newer-than-retention".into(),
            kind: SessionWorkflowKind::Compact(ProductCompactReason::Manual),
            source_locator: locator,
        };
        store.write_workflow_request(&id, &newer).unwrap();
        let intents = store.load_intents(&id).unwrap();
        assert_eq!(intents.workflow_anchors[&newer.operation_id], 10);
        assert_eq!(
            plakous_hosted::sessions::projector::pending_workflow_operation_ids(
                &store.load_views(&id).unwrap().unwrap(),
                &intents,
            ),
            vec![newer.operation_id],
        );
    }

    fn views_at(cursor: u64, rows: &[(u64, TranscriptRow)]) -> SessionViews {
        let mut views = SessionViews::empty();
        views.cursor = FeedSeq::new(cursor);
        views.state = SessionViewState {
            runtime_session_id: Some(uuid(1).parse().unwrap()),
            counters: Counters {
                root_turns: cursor,
                tool_calls: 1,
                compactions: 0,
            },
            ..SessionViewState::default()
        };
        for (anchor, row) in rows {
            views
                .rows
                .insert(RowKey::new(*anchor, Lane::Runtime, 0), row.clone());
        }
        views
    }

    #[test]
    fn clear_intent_and_genesis_successor_are_one_atomic_cas_write() {
        let root = tempdir().unwrap();
        let (mut store, id) = store_with_session(root.path());
        let mut source = store.load(&id).unwrap();
        let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
        source.bind_plakous_locator(locator).unwrap();
        store.commit(&mut source).unwrap();
        let request = WorkflowRequest {
            operation_id: "plak:clear:atomic-intent".into(),
            source_locator: locator,
            kind: SessionWorkflowKind::Clear,
        };
        let target_id = request.successor_product_id();
        store.connection.execute_batch(&format!(
            "CREATE TEMP TRIGGER refuse_clear BEFORE INSERT ON sessions WHEN NEW.id='{target_id}' BEGIN SELECT RAISE(ABORT, 'cut'); END;"
        )).unwrap();
        let revision = source.store_revision;
        assert!(
            store
                .submit_workflow_request(&mut source, &request)
                .is_err()
        );
        assert_eq!(source.store_revision, revision);
        assert_eq!(store.load(&id).unwrap().store_revision, revision);
        assert_eq!(
            store.load_intents(&id).unwrap().workflow_requests,
            [] as [WorkflowRequest; 0]
        );
        assert!(store.load(&target_id).is_err());
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_clear")
            .unwrap();
        store
            .submit_workflow_request(&mut source, &request)
            .unwrap();
        let target = store.load(&target_id).unwrap();
        assert_eq!(
            target.plakous_locator,
            Some(
                plakous::sessions::AgentSessions::genesis_locator_for(
                    &plakous::sessions::SessionOperationId::new(&target.create_operation_id)
                        .unwrap(),
                )
                .unwrap()
            )
        );
        assert!(target.transcript.is_empty());
        assert_eq!(target.prompt_history, [] as [String; 0]);
    }

    fn id_from<T: std::str::FromStr>(n: u64) -> T
    where
        T::Err: std::fmt::Debug,
    {
        uuid(n).parse().unwrap()
    }

    #[test]
    fn clear_withdrawal_rolls_back_reservation_deletion_and_keeps_identity_bindings() {
        let root = tempdir().unwrap();
        let (mut store, id) = store_with_session(root.path());
        let mut source = store.load(&id).unwrap();
        let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
        source.bind_plakous_locator(locator).unwrap();
        store.commit(&mut source).unwrap();
        let request = WorkflowRequest {
            operation_id: "plak:clear:withdrawal-cut".into(),
            source_locator: locator,
            kind: SessionWorkflowKind::Clear,
        };
        store
            .submit_workflow_request(&mut source, &request)
            .unwrap();
        let target = store.load(&request.successor_product_id()).unwrap();
        let empty = SessionViews::empty();
        store
            .commit_projection_views(&id, &ViewWrites::between(&empty, &empty), &empty)
            .unwrap();
        store.connection.execute_batch("CREATE TEMP TRIGGER refuse_clear_notice BEFORE INSERT ON operator_notices BEGIN SELECT RAISE(ABORT, 'cut'); END;").unwrap();
        let revision = source.store_revision;
        assert!(store.withdraw_unadmitted_workflows(&mut source).is_err());
        assert_eq!(source.store_revision, revision);
        assert_eq!(store.load(&id).unwrap().store_revision, revision);
        assert!(store.load(&target.id).is_ok());
        assert_eq!(
            store.load_intents(&id).unwrap().workflow_requests,
            vec![request]
        );
        assert_eq!(
            store.load_intents(&id).unwrap().notices,
            [] as [OperatorNotice; 0]
        );
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_clear_notice")
            .unwrap();
        assert_eq!(
            store
                .withdraw_unadmitted_workflows(&mut source)
                .unwrap()
                .len(),
            1
        );
        assert!(store.load(&target.id).is_err());
        let bound: bool = store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM identity_bindings WHERE identity=?1 AND owner_session_id=?2)",
            params![target.create_operation_id, target.id], |row| row.get(0),
        ).unwrap();
        assert!(
            bound,
            "deleting an uncreated reservation never releases its identity"
        );
    }

    #[test]
    fn clear_creation_join_is_atomic_rebuildable_and_survives_same_cursor_writers() {
        let root = tempdir().unwrap();
        let (mut store, id) = store_with_session(root.path());
        let mut source = store.load(&id).unwrap();
        let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
        source.bind_plakous_locator(locator).unwrap();
        store.commit(&mut source).unwrap();
        let request = WorkflowRequest {
            operation_id: "plak:clear:atomic-join".into(),
            source_locator: locator,
            kind: SessionWorkflowKind::Clear,
        };
        store
            .submit_workflow_request(&mut source, &request)
            .unwrap();
        let target = store.load(&request.successor_product_id()).unwrap();
        let mut views = SessionViews::empty();
        views.cursor = FeedSeq::new(1);
        views.state.created_operation_id = Some(target.create_operation_id.clone());
        views.state.runtime_session_id = target.plakous_locator.map(SessionLocator::session_id);
        let writes = ViewWrites::between(&SessionViews::empty(), &views);
        store.connection.execute_batch(&format!(
            "CREATE TEMP TRIGGER refuse_clear_join BEFORE INSERT ON session_views WHEN NEW.session_id='{id}' BEGIN SELECT RAISE(ABORT, 'cut'); END;"
        )).unwrap();
        assert!(
            store
                .commit_projection_views(&target.id, &writes, &views)
                .is_err()
        );
        assert!(store.load_views(&target.id).unwrap().is_none());
        assert!(store.load_views(&id).unwrap().is_none());
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_clear_join")
            .unwrap();
        store
            .commit_projection_views(&target.id, &writes, &views)
            .unwrap();
        let source_views = store.load_views(&id).unwrap().unwrap();
        assert!(
            source_views.state.workflows[&request.operation_id]
                .settled
                .is_some()
        );
        let stale = SessionViews::empty();
        store
            .commit_projection_views(&id, &ViewWrites::between(&source_views, &stale), &stale)
            .unwrap();
        assert_eq!(store.load_views(&id).unwrap().unwrap(), source_views);
        store.delete_views(&id).unwrap();
        store
            .commit_projection_views(&target.id, &ViewWrites::between(&views, &views), &views)
            .unwrap();
        assert_eq!(store.load_views(&id).unwrap().unwrap(), source_views);
        assert_eq!(
            store.withdraw_unadmitted_workflows(&mut source).unwrap(),
            [] as [TranscriptRow; 0]
        );
    }

    #[test]
    fn fork_successor_publication_is_atomic_idempotent_and_copies_only_prefix_notices() {
        use plakous::sessions::feed::{FeedBranchTransition, FeedLocator};
        use plakous_hosted::sessions::projector::Successor;

        let root = tempdir().unwrap();
        let (mut store, id) = store_with_session(root.path());
        let source = SessionLocator::new(
            uuid(1).parse().unwrap(),
            uuid(2).parse().unwrap(),
            uuid(3).parse().unwrap(),
        );
        let mut record = store.load(&id).unwrap();
        record.bind_plakous_locator(source).unwrap();
        store.commit(&mut record).unwrap();
        let request = WorkflowRequest {
            operation_id: "plak:fork:atomic".into(),
            source_locator: source,
            kind: SessionWorkflowKind::Fork,
        };
        store
            .submit_workflow_request(&mut record, &request)
            .unwrap();
        for anchor in [2, 5, 6] {
            insert_notice(
                &store.connection,
                &id,
                &OperatorNotice {
                    notice_id: format!("notice-{anchor}"),
                    anchor,
                    ordinal: 0,
                    row: TranscriptRow::info(NoticeScope::Session, format!("notice {anchor}")),
                },
            )
            .unwrap();
        }
        let source_views = views_at(5, &[(1, TranscriptRow::user("prefix"))]);
        let mut target_views = source_views.clone();
        target_views.state.head = Some(FeedLocator {
            session_id: source.session_id(),
            branch_id: uuid(4).parse().unwrap(),
            checkpoint_id: uuid(5).parse().unwrap(),
        });
        let mut writes = ViewWrites::between(&SessionViews::empty(), &source_views);
        writes.successors.push(Successor {
            operation_id: Some(request.operation_id.clone()),
            transition: FeedBranchTransition::Fork,
            target: target_views.state.head.unwrap(),
            published_seq: source_views.cursor,
            created_at_unix: 42,
        });
        let target_id = request.successor_product_id();
        store.connection.execute_batch(&format!(
            "CREATE TEMP TRIGGER refuse_successor BEFORE INSERT ON sessions WHEN NEW.id='{target_id}' BEGIN SELECT RAISE(ABORT, 'cut'); END;"
        )).unwrap();
        assert!(
            store
                .commit_projection_views(&id, &writes, &source_views)
                .is_err()
        );
        assert!(store.load_views(&id).unwrap().is_none());
        assert!(store.load(&target_id).is_err());
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_successor")
            .unwrap();
        assert_eq!(
            store
                .commit_projection_views(&id, &writes, &source_views)
                .unwrap(),
            ProjectionCommit::Applied
        );
        assert_eq!(
            store
                .commit_projection_views(&id, &writes, &source_views)
                .unwrap(),
            ProjectionCommit::CursorMoved
        );
        let target = store.load(&target_id).unwrap();
        assert_eq!(target.created_at_unix, 42);
        assert_eq!(
            target.source_product_session_id.as_deref(),
            Some(id.as_str())
        );

        assert!(store.load_views(&target_id).unwrap().is_none());
        let notices = store.load_intents(&target_id).unwrap().notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].anchor, 2);
        store.delete_views(&id).unwrap();
        assert_eq!(
            store
                .commit_projection_views(&id, &writes, &source_views)
                .unwrap(),
            ProjectionCommit::Applied
        );
        assert!(store.load_views(&target_id).unwrap().is_none());
        assert_eq!(store.load_intents(&target_id).unwrap().notices, notices);
        assert_eq!(
            store.load(&target_id).unwrap().store_revision,
            target.store_revision
        );
    }

    fn writes(from: &SessionViews, to: &SessionViews) -> ViewWrites {
        ViewWrites::between(from, to)
    }

    #[test]
    fn long_session_projection_write_measurement() {
        use plakous_hosted::sessions::projector::CallView;
        let root = tempdir().unwrap();
        let (mut store, session) = store_with_session(root.path());
        let mut views = views_at(1, &[]);
        for n in 0..2_000 {
            views.state.calls.insert(
                format!("call-{n}"),
                CallView {
                    key: RowKey::new(n + 1, Lane::Runtime, 0),
                    turn_id: Some("turn-1".to_owned()),
                    counted: true,
                    operated: true,
                    settled: true,
                },
            );
        }
        store
            .commit_projection_views(&session, &writes(&SessionViews::empty(), &views), &views)
            .unwrap();
        let start = std::time::Instant::now();
        let mut state_bytes = 0;
        let before = store.connection.total_changes();
        for _ in 0..100 {
            let from = views.clone();
            views.cursor = FeedSeq::new(views.cursor.get() + 1);
            views.state.calls.get_mut("call-1999").unwrap().settled ^= true;
            store
                .commit_projection_views(&session, &writes(&from, &views), &views)
                .unwrap();
            state_bytes = store
                .connection
                .query_row(
                    "SELECT length(state) FROM session_views WHERE session_id=?1",
                    [&session],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap();
        }
        eprintln!(
            "L5_MEASUREMENT calls=2000 commits=100 elapsed_us={} rows_changed={} state_bytes={state_bytes}",
            start.elapsed().as_micros(),
            store.connection.total_changes() - before
        );
    }

    #[test]
    fn keyed_bookkeeping_is_not_in_the_session_state_document() {
        let state = serde_json::to_value(SessionViewState::default()).unwrap();
        for field in ["calls", "open_turns", "processes"] {
            assert!(state.get(field).is_none(), "{field} must be a keyed table");
        }
    }

    fn with_bookkeeping(mut views: SessionViews) -> SessionViews {
        for n in 0..2 {
            views.state.calls.insert(
                format!("call-{n}"),
                CallView {
                    key: RowKey::new(n + 1, Lane::Runtime, 0),
                    turn_id: Some(format!("turn-{n}")),
                    counted: true,
                    operated: true,
                    settled: false,
                },
            );
            views.state.open_turns.insert(
                format!("turn-{n}"),
                OpenTurn {
                    command_id: format!("command-{n}"),
                    host_wake: false,
                    round_anchor: Some(n + 1),
                    cancel_requested: false,
                },
            );
            views.state.processes.insert(
                n,
                ProcessTrack {
                    dispatched_at_ms: Some(n * 100),
                    reported: false,
                },
            );
        }
        views
    }

    #[test]
    fn keyed_views_round_trip_change_only_touched_keys_and_rebuild_under_cas() {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        let (mut store, session) = store_with_session(root.path());
        assert_eq!(super::super::SCHEMA_VERSION, 32);
        let empty = SessionViews::empty();
        let first = with_bookkeeping(views_at(1, &[]));
        store
            .commit_projection_views(&session, &writes(&empty, &first), &first)
            .unwrap();
        assert_eq!(store.load_views(&session).unwrap(), Some(first.clone()));
        let mut second = first.clone();
        second.cursor = FeedSeq::new(2);
        second.state.calls.remove("call-0");
        second.state.calls.get_mut("call-1").unwrap().settled = true;
        second.state.open_turns.remove("turn-0");
        second
            .state
            .open_turns
            .get_mut("turn-1")
            .unwrap()
            .cancel_requested = true;
        second.state.processes.remove(&0);
        second.state.processes.get_mut(&1).unwrap().reported = true;
        let before = store.connection.total_changes();
        store
            .commit_projection_views(&session, &writes(&first, &second), &second)
            .unwrap();
        assert_eq!(store.connection.total_changes() - before, 8);
        let before = store.connection.total_changes();
        assert_eq!(
            store
                .commit_projection_views(&session, &writes(&first, &second), &second)
                .unwrap(),
            ProjectionCommit::CursorMoved
        );
        assert_eq!(store.connection.total_changes(), before);
        drop(store);
        let mut store = ProductStore::open(&paths).unwrap();
        assert_eq!(store.load_views(&session).unwrap(), Some(second.clone()));
        store.delete_views(&session).unwrap();
        for table in [
            "session_call_views",
            "session_open_turn_views",
            "session_process_views",
        ] {
            assert_eq!(
                store
                    .connection
                    .query_row(
                        &format!("SELECT count(*) FROM {table} WHERE session_id=?1"),
                        [&session],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
        }
        store
            .commit_projection_views(&session, &writes(&empty, &second), &second)
            .unwrap();
        assert_eq!(store.load_views(&session).unwrap(), Some(second));
    }

    #[test]
    fn a_failed_keyed_write_rolls_back_the_cursor_and_all_views() {
        let root = tempdir().unwrap();
        let (mut store, session) = store_with_session(root.path());
        let empty = SessionViews::empty();
        let first = with_bookkeeping(views_at(1, &[]));
        store
            .commit_projection_views(&session, &writes(&empty, &first), &first)
            .unwrap();
        store.connection.execute_batch("CREATE TEMP TRIGGER fail_process_update BEFORE UPDATE ON session_process_views BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        let mut second = first.clone();
        second.cursor = FeedSeq::new(2);
        second.state.calls.get_mut("call-1").unwrap().settled = true;
        second.state.processes.get_mut(&1).unwrap().reported = true;
        assert!(
            store
                .commit_projection_views(&session, &writes(&first, &second), &second)
                .is_err()
        );
        assert_eq!(store.load_views(&session).unwrap(), Some(first));
    }

    #[test]
    fn keyed_views_are_session_scoped_and_cascade_on_session_deletion() {
        let root = tempdir().unwrap();
        let (mut store, first) = store_with_session(root.path());
        let mut second = SessionRecord::new(PathBuf::from("/work"));
        store.commit(&mut second).unwrap();
        let views = with_bookkeeping(views_at(1, &[]));
        let writes = writes(&SessionViews::empty(), &views);
        for session in [&first, &second.id] {
            store
                .commit_projection_views(session, &writes, &views)
                .unwrap();
        }
        // Schema 31 rejects the obsolete JSON shape even through raw SQL.
        for field in ["calls", "open_turns", "processes"] {
            assert!(store.connection.execute(
                "UPDATE session_views SET state=json_set(state,?2,json('{}')) WHERE session_id=?1",
                params![first, format!("$.{field}")],
            ).is_err());
        }
        let revision = store.load(&first).unwrap().store_revision;
        store.delete_session(&first, revision).unwrap();
        assert_eq!(store.load_views(&first).unwrap(), None);
        assert_eq!(store.load_views(&second.id).unwrap(), Some(views));
        for table in [
            "session_call_views",
            "session_open_turn_views",
            "session_process_views",
        ] {
            assert_eq!(
                store
                    .connection
                    .query_row(
                        &format!("SELECT count(*) FROM {table} WHERE session_id=?1"),
                        [&first],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn intents_round_trip_by_session() {
        let root = tempdir().unwrap();
        let (mut store, session) = store_with_session(root.path());
        let submission = Submission {
            command_id: "cmd-1".to_owned(),
            kind: SubmissionKind::RootStart,
            queue_item_id: "item-1".to_owned(),
            queue_item_revision: 2,
            target_turn_id: None,
            canonical_command: "{\"prompt\":\"hi\"}".to_owned(),
            submitted_at_ms: 1_234,
        };
        store.write_submission(&session, &submission).unwrap();
        store.write_submission(&session, &submission).unwrap();
        let conflicting = Submission {
            canonical_command: "other".to_owned(),
            ..submission.clone()
        };
        assert!(store.write_submission(&session, &conflicting).is_err());
        let request = WorkflowRequest {
            operation_id: "plak:compact:1".to_owned(),
            kind: SessionWorkflowKind::Compact(ProductCompactReason::Manual),
            source_locator: SessionLocator::new(
                uuid(1).parse().unwrap(),
                uuid(2).parse().unwrap(),
                uuid(3).parse().unwrap(),
            ),
        };
        store.write_workflow_request(&session, &request).unwrap();
        store.write_workflow_request(&session, &request).unwrap();
        write_import_intents(
            &store.connection,
            &session,
            &[TranscriptRow::user("carried")],
        )
        .unwrap();
        let notice = OperatorNotice {
            notice_id: "n-1".to_owned(),
            anchor: 4,
            ordinal: 0,
            row: TranscriptRow::warning(NoticeScope::Queue, "compaction did not start"),
        };
        insert_notice(&store.connection, &session, &notice).unwrap();
        store
            .write_wake_suppression(
                &session,
                &AnchoredIntent {
                    anchor: 4,
                    ordinal: 1,
                    value: (),
                },
            )
            .unwrap();
        let pause = AnchoredIntent {
            anchor: 5,
            ordinal: 0,
            value: HostPauseCause::WakePreflightFailed {
                reason: "tree store unavailable".to_owned(),
            },
        };
        store.write_pause_cause(&session, &pause).unwrap();
        let intents = store.load_intents(&session).unwrap();
        assert_eq!(intents.submissions, vec![submission]);
        assert_eq!(intents.workflow_requests, vec![request]);
        assert_eq!(intents.notices.len(), 2);
        assert_eq!(intents.notices[0].anchor, 0);
        assert_eq!(intents.notices[1], notice);
        assert_eq!(intents.wake_suppressions.len(), 1);
        assert_eq!(intents.pause_causes, vec![pause]);
        let mut record = store.load(&session).unwrap();
        store
            .discard_submission(&mut record, "cmd-1", None)
            .unwrap();
        store
            .commit_projection_views(
                &session,
                &ViewWrites::between(&SessionViews::empty(), &views_at(0, &[])),
                &views_at(0, &[]),
            )
            .unwrap();
        let notices = store.withdraw_unadmitted_workflows(&mut record).unwrap();
        assert_eq!(notices.len(), 1);
        let intents = store.load_intents(&session).unwrap();
        assert!(intents.submissions.is_empty() && intents.workflow_requests.is_empty());
        assert_eq!(intents.notices.len(), 3);
        assert_eq!(
            store.withdraw_unadmitted_workflows(&mut record).unwrap(),
            [] as [TranscriptRow; 0]
        );
    }

    /// Spec 02 §2.6: the counters are views. The live record's counters are
    /// display state; load and listing read only the committed views.
    #[test]
    fn counters_load_and_list_only_from_committed_views() {
        let root = tempdir().unwrap();
        let mut store = ProductStore::open(&PlakPaths::explicit(root.path().join("p"))).unwrap();
        let mut session = SessionRecord::new(PathBuf::from("/work"));
        session.root_turn_count = 9;
        session.tool_call_count = 9;
        session.compaction_count = 9;
        store.commit(&mut session).unwrap();
        let counters = |record: &SessionRecord| {
            (
                record.root_turn_count,
                record.tool_call_count,
                record.compaction_count,
            )
        };
        assert_eq!(counters(&store.load(&session.id).unwrap()), (0, 0, 0));
        let views = views_at(2, &[(1, TranscriptRow::user("hello"))]);
        store
            .commit_projection_views(&session.id, &writes(&SessionViews::empty(), &views), &views)
            .unwrap();
        assert_eq!(counters(&store.load(&session.id).unwrap()), (2, 1, 0));
        let listed = store.list_for_workspace(Path::new("/work")).unwrap();
        assert_eq!(counters(&listed[0]), (2, 1, 0));
    }

    #[test]
    fn a_projection_commits_by_cursor_cas_and_a_stale_applier_changes_nothing() {
        let root = tempdir().unwrap();
        let (mut store, session) = store_with_session(root.path());
        assert_eq!(store.load_views(&session).unwrap(), None);
        let empty = SessionViews::empty();
        let first = views_at(
            2,
            &[
                (1, TranscriptRow::user("hello")),
                (2, TranscriptRow::assistant("hi")),
            ],
        );
        assert_eq!(
            store
                .commit_projection_views(&session, &writes(&empty, &first), &first)
                .unwrap(),
            ProjectionCommit::Applied
        );
        assert_eq!(store.load_views(&session).unwrap().as_ref(), Some(&first));
        // A concurrent applier that read the empty cursor loses the CAS.
        let other = views_at(1, &[(1, TranscriptRow::user("stale"))]);
        assert_eq!(
            store
                .commit_projection_views(&session, &writes(&empty, &other), &other)
                .unwrap(),
            ProjectionCommit::CursorMoved
        );
        let mut second = views_at(3, &[(3, TranscriptRow::assistant("more"))]);
        second.rows.insert(
            RowKey::new(2, Lane::Runtime, 0),
            TranscriptRow::assistant("hi"),
        );
        assert_eq!(
            store
                .commit_projection_views(&session, &writes(&first, &second), &second)
                .unwrap(),
            ProjectionCommit::Applied
        );
        assert_eq!(store.load_views(&session).unwrap(), Some(second.clone()));
        // Deleting the views is always safe; a rebuild from empty writes the
        // same views.
        store.delete_views(&session).unwrap();
        assert_eq!(store.load_views(&session).unwrap(), None);
        store
            .commit_projection_views(&session, &writes(&empty, &second), &second)
            .unwrap();
        assert_eq!(store.load_views(&session).unwrap(), Some(second));
    }

    /// Spec 02 §2.6: nothing but the projector writes views. Every statement
    /// that writes a view table lives in this module, behind
    /// `commit_projection_views` and `delete_views`.
    #[test]
    fn only_the_projector_commit_writes_view_tables() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let roots = [
            manifest.join("src"),
            manifest.join("../../crates/hosting/hosted/src"),
        ];
        let own = manifest.join("src/store/views.rs");
        let mut offenders = Vec::new();
        let mut stack: Vec<PathBuf> = roots.to_vec();
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                for entry in std::fs::read_dir(&path).unwrap() {
                    stack.push(entry.unwrap().path());
                }
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs")
                || path.canonicalize().unwrap() == own.canonicalize().unwrap()
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for table in [
                "transcript_rows",
                "session_views",
                "projector_cursors",
                "session_call_views",
                "session_open_turn_views",
                "session_process_views",
            ] {
                for verb in ["INSERT INTO ", "UPDATE ", "DELETE FROM ", "REPLACE INTO "] {
                    if text.contains(&format!("{verb}{table}")) {
                        offenders.push(format!("{}: {verb}{table}", path.display()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "view writers outside the projector commit: {offenders:?}"
        );
    }
}
