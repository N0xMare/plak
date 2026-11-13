//! Schema 31 intents and projector views (spec 02 §2.2, §2.6).
//!
//! Intent writes commit under `synchronous=FULL`; a projector transaction
//! commits under `NORMAL` and advances the cursor by CAS from the value its
//! run read, so a concurrent applier's transaction changes nothing. This is
//! the only module that writes the projector view tables.

use plakous_hosted::session_store::RecordStoreError;

use std::collections::{BTreeMap, HashMap};

use plakous::sessions::SessionLocator;
use plakous::sessions::feed::FeedSeq;
use plakous_hosted::sessions::projector::{
    AnchoredIntent, HostPauseCause, Lane, OperatorNotice, ProductIntents, ProjectionCommit,
    RootContinuation, RootInterrupt, RowKey, SessionViewState, SessionViews, Submission,
    SubmissionKind, ViewWrites, WorkflowRequest,
};
use plakous_hosted::sessions::{TranscriptRow, encode_workflow_kind};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, TransactionState, params};

use super::{ProductStore, decode_workflow_kind, from_i64, to_i64};

const WAKE_SUPPRESSION: &str = "wake_suppression";
const PAUSE_CAUSE: &str = "pause_cause";
const ROOT_INTERRUPT: &str = "interrupt_cause";
const ROOT_CONTINUATION: &str = "continuation_decision";

fn invalid(_: rusqlite::Error) -> String {
    "product view row is invalid".to_owned()
}

fn unavailable(_: rusqlite::Error) -> String {
    "product store is unavailable".to_owned()
}

fn projection_refusal(error: rusqlite::Error) -> Result<ProjectionCommit, RecordStoreError> {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) {
        return Ok(ProjectionCommit::CursorMoved);
    }
    Err(super::write_error(error, "product projection write failed"))
}

impl ProductStore {
    pub fn load_inherited_submissions(&mut self, id: &str) -> Result<Vec<Submission>, String> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(unavailable)?;
        let submissions = inherited_submissions_of(&tx, &mut self.verified_submissions, id)?;
        tx.commit().map_err(unavailable)?;
        Ok(submissions)
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

    /// Construction fields, intents and views from one read-only
    /// snapshot. No legacy delivery, workflow or head-adoption state is read.
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
            let verified = &mut VerifiedSubmissions::default();
            let mut intents = intents_of(snapshot, verified, session_id)?;
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
                    .extend(intents_of(snapshot, verified, &source)?.submissions);
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
    ) -> Result<(), RecordStoreError> {
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        super::lifecycle::admit_work(&tx, session_id)?;
        insert_submission(&tx, &mut self.verified_submissions, session_id, submission)?;
        tx.commit()
            .map_err(|error| super::write_error(error, "product store write failed"))
    }

    /// Writes one workflow request before it is submitted.
    pub fn write_workflow_request(
        &mut self,
        session_id: &str,
        request: &WorkflowRequest,
    ) -> Result<(), RecordStoreError> {
        self.durability(true)?;
        let (reason, selection) = encode_workflow_kind(&request.kind);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        super::lifecycle::admit_work(&tx, session_id)?;
        let changed = tx.execute(
                "INSERT INTO workflow_requests(session_id,operation_id,source_locator,kind,compact_reason,selection,anchor) VALUES(?1,?2,?3,?4,?5,?6,COALESCE((SELECT applied_seq FROM projector_cursors WHERE session_id=?1),0)) ON CONFLICT(operation_id) DO NOTHING",
                params![
                    session_id,
                    request.operation_id,
                    request.source_locator.to_string(),
                    request.kind.as_str(),
                    reason,
                    selection,
                ],
            )
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        tx.commit()
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        if changed == 0 {
            let existing = self.load_intents(session_id)?;
            if existing.workflow_request(&request.operation_id) != Some(request) {
                return Err(format!(
                    "workflow operation id {} is already bound to another request",
                    request.operation_id
                )
                .into());
            }
        }
        Ok(())
    }

    /// Writes a live request under the authoring session's revision CAS.
    pub fn submit_workflow_request(
        &mut self,
        session: &mut plakous_hosted::sessions::SessionRecord,
        request: &WorkflowRequest,
    ) -> Result<(), RecordStoreError> {
        plakous::sessions::SessionOperationId::new(&request.operation_id).map_err(|error| {
            super::write_error(error, "workflow operation identity is invalid".to_owned())
        })?;
        if session.plakous_locator != Some(request.source_locator) {
            return Err("workflow source is not the exact product binding"
                .to_owned()
                .into());
        }
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        super::lifecycle::admit_work(&tx, &session.id)?;
        let (reason, selection) = encode_workflow_kind(&request.kind);
        let inserted = tx.execute(
            "INSERT INTO workflow_requests(session_id,operation_id,source_locator,kind,compact_reason,selection,anchor) VALUES(?1,?2,?3,?4,?5,?6,COALESCE((SELECT applied_seq FROM projector_cursors WHERE session_id=?1),0)) ON CONFLICT(operation_id) DO NOTHING",
            params![session.id, request.operation_id, request.source_locator.to_string(), request.kind.as_str(), reason, selection],
        ).map_err(|error| super::write_error(error, "product store write failed"))?;
        if inserted == 0
            && intents_of(&tx, &mut self.verified_submissions, &session.id)?
                .workflow_request(&request.operation_id)
                != Some(request)
        {
            return Err("workflow identity is bound to another request"
                .to_owned()
                .into());
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
        if request.kind == plakous_hosted::sessions::SessionWorkflowKind::New {
            let target = session.new_session_from_request(request)?;
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    [&target.id],
                    |row| row.get(0),
                )
                .map_err(|error| super::write_error(error, "product store write failed"))?;
            if exists {
                let existing = super::load_record(&tx, &mut self.verified_submissions, &target.id)?;
                if existing.create_operation_id != target.create_operation_id
                    || existing.source_product_session_id != target.source_product_session_id
                    || existing.source_workflow_operation_id != target.source_workflow_operation_id
                {
                    return Err("clear successor identity is bound to another request"
                        .to_owned()
                        .into());
                }
            } else {
                super::commit_in_transaction(&tx, &target, 0, None)?;
            }
        }
        tx.commit()
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        session.store_revision = next;
        session.touch();
        Ok(())
    }

    /// Removes a definitively unadmitted live request under the session CAS.
    pub fn discard_unadmitted_workflow(
        &mut self,
        session: &mut plakous_hosted::sessions::SessionRecord,
        operation_id: &str,
    ) -> Result<(), RecordStoreError> {
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        let views = views_of(&tx, &session.id)?
            .ok_or_else(|| "workflow discard requires projected views".to_owned())?;
        let intents = intents_of(&tx, &mut self.verified_submissions, &session.id)?;
        let Some(request) = intents.workflow_request(operation_id) else {
            return Ok(());
        };
        if !plakous_hosted::sessions::projector::pending_workflow_operation_ids(&views, &intents)
            .iter()
            .any(|id| id == operation_id)
        {
            return Err("a recorded workflow request cannot be discarded"
                .to_owned()
                .into());
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
        .map_err(|error| super::write_error(error, "product store write failed"))?;
        tx.commit()
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        session.store_revision = next;
        session.touch();
        Ok(())
    }

    /// Withdraws unadmitted requests and their notices in one FULL commit.
    pub fn withdraw_unadmitted_workflows(
        &mut self,
        session: &mut plakous_hosted::sessions::SessionRecord,
    ) -> Result<Vec<TranscriptRow>, RecordStoreError> {
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        let views = views_of(&tx, &session.id)?
            .ok_or_else(|| "workflow withdrawal requires projected views".to_owned())?;
        let intents = intents_of(&tx, &mut self.verified_submissions, &session.id)?;
        let withdrawals =
            plakous_hosted::sessions::projector::workflow_withdrawals(&views, &intents);
        if withdrawals.is_empty() {
            return Ok(Vec::new());
        }
        let next = workflow_intent_cas(&tx, session)?;
        let mut rows = Vec::new();
        for (id, notice) in withdrawals {
            if let Some(request) = intents.workflow_request(&id)
                && request.kind == plakous_hosted::sessions::SessionWorkflowKind::New
            {
                let target_id = request.successor_product_id();
                let target =
                    match super::load_record(&tx, &mut self.verified_submissions, &target_id) {
                        Ok(target) => Some(target),
                        Err(error) if error == "session not found" => None,
                        Err(error) => return Err(error.into()),
                    };
                if let Some(target) = target {
                    if views_of(&tx, &target_id)?.is_some()
                        || target.source_product_session_id.as_deref() != Some(session.id.as_str())
                        || target.source_workflow_operation_id.as_deref() != Some(id.as_str())
                        || !target.prompt_history.is_empty()
                        || !target.pending_queue.is_empty()
                        || intents_of(&tx, &mut self.verified_submissions, &target_id)?
                            != ProductIntents::default()
                    {
                        return Err("unadmitted clear successor is not an empty reservation".into());
                    }
                    // The successful source pass proved this reserved genesis has
                    // no runtime session. Identity bindings deliberately survive.
                    tx.execute("DELETE FROM sessions WHERE id=?1", [&target_id])
                        .map_err(|error| super::write_error(error, "product store write failed"))?;
                    self.verified_submissions.forget_session(&target_id);
                }
            }
            tx.execute(
                "DELETE FROM workflow_requests WHERE session_id=?1 AND operation_id=?2",
                params![session.id, id],
            )
            .map_err(|error| super::write_error(error, "product store write failed"))?;
            insert_notice(&tx, &session.id, &notice)?;
            rows.push(notice.row);
        }
        tx.commit()
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        session.store_revision = next;
        session.touch();
        Ok(rows)
    }

    /// Writes a child-wake suppression intent.
    pub fn write_wake_suppression(
        &mut self,
        session_id: &str,
        intent: &AnchoredIntent<()>,
    ) -> Result<(), RecordStoreError> {
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
    ) -> Result<(), RecordStoreError> {
        let value = serde_json::to_string(&intent.value).map_err(|error| {
            super::write_error(error, "pause cause is not encodable".to_owned())
        })?;
        self.write_host_intent(
            session_id,
            PAUSE_CAUSE,
            intent.anchor,
            intent.ordinal,
            value,
        )
    }

    /// Writes a root interrupt cause at an explicit anchor.
    pub fn write_root_interrupt(
        &mut self,
        session_id: &str,
        intent: &AnchoredIntent<RootInterrupt>,
    ) -> Result<(), RecordStoreError> {
        let value = intent.value.encode()?;
        self.write_host_intent(
            session_id,
            ROOT_INTERRUPT,
            intent.anchor,
            intent.ordinal,
            value,
        )
    }

    /// Writes a root continuation decision at an explicit anchor.
    pub fn write_root_continuation(
        &mut self,
        session_id: &str,
        intent: &AnchoredIntent<RootContinuation>,
    ) -> Result<(), RecordStoreError> {
        let value = intent.value.encode()?;
        self.write_host_intent(
            session_id,
            ROOT_CONTINUATION,
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
    ) -> Result<(), RecordStoreError> {
        self.durability(true)?;
        self.connection
            .execute(
                "INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,?5)",
                params![session_id, kind, to_i64(anchor)?, ordinal, value],
            )
            .map_err(|error| super::write_error(error, format!("could not record the {kind} intent")))?;
        Ok(())
    }

    /// Every intent of one session the projector reads.
    pub fn load_intents(&mut self, session_id: &str) -> Result<ProductIntents, String> {
        intents_of(&self.connection, &mut self.verified_submissions, session_id)
    }

    pub(crate) fn submission_owner(&mut self, command_id: &str) -> Result<Option<String>, String> {
        let submissions = load_submissions_where(
            &self.connection,
            &mut self.verified_submissions,
            "command_id=?1",
            command_id,
        )?;
        Ok(submissions.into_iter().next().map(|(owner, _)| owner))
    }

    /// The stored views of one session, or `None` before its first projector
    /// transaction (or after the views were deleted for a rebuild).
    pub fn load_views(&mut self, session_id: &str) -> Result<Option<SessionViews>, String> {
        views_of(&self.connection, session_id)
    }

    /// The queue join reads submissions and delivery state in one snapshot,
    /// without decoding transcript rows.
    pub fn load_queue_commands(
        &mut self,
        session_id: &str,
    ) -> Result<plakous_hosted::sessions::QueueCommands, String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(unavailable)?;
        let commands = queue_commands_of(&transaction, &mut self.verified_submissions, session_id)?;
        transaction.commit().map_err(unavailable)?;
        Ok(commands)
    }

    /// Commits one projector run: CAS-advances the cursor from the value the
    /// run read and writes the changed rows and the view state in the same
    /// transaction, under `synchronous=NORMAL`.
    #[allow(
        clippy::too_many_lines,
        reason = "Projection publication atomically checks revision and delivery identity before installing the session, transcript, queue, and workflow views."
    )]
    pub fn commit_projection_views(
        &mut self,
        session_id: &str,
        writes: &ViewWrites,
        views: &SessionViewState,
    ) -> Result<ProjectionCommit, RecordStoreError> {
        #[cfg(test)]
        self.capacity_fault()?;
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
        let intents = intents_of(&tx, &mut self.verified_submissions, session_id)?;
        for request in &intents.workflow_requests {
            if request.kind != plakous_hosted::sessions::SessionWorkflowKind::New
                || intents.workflow_consumed(&request.operation_id, views.retained_from)
                || views
                    .workflows
                    .get(&request.operation_id)
                    .is_some_and(|workflow| workflow.settled.is_some())
            {
                continue;
            }
            let target_id = request.successor_product_id();
            if let Some(target_views) = views_of(&tx, &target_id)? {
                let target = super::load_record(&tx, &mut self.verified_submissions, &target_id)?;
                if let Some(completion) =
                    plakous_hosted::sessions::projector::new_session_completion(
                        request,
                        &target,
                        &target_views.state,
                    )
                {
                    joined
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
            .created_operation_id
            .as_deref()
            .is_some_and(|operation| operation.ends_with(":create"))
        {
            let target = super::load_record(&tx, &mut self.verified_submissions, session_id)?;
            if let (Some(source_id), Some(operation)) = (
                target.source_product_session_id.as_deref(),
                target.source_workflow_operation_id.as_deref(),
            ) {
                let intents = intents_of(&tx, &mut self.verified_submissions, source_id)?;
                if let Some(completion) = intents.workflow_request(operation).and_then(|request| {
                    plakous_hosted::sessions::projector::new_session_completion(
                        request, &target, views,
                    )
                }) {
                    let previous = views_of(&tx, source_id)?.unwrap_or_else(SessionViews::empty);
                    let mut source_views = previous.clone();
                    source_views
                        .state
                        .workflows
                        .insert(operation.to_owned(), completion);
                    source_views.consume_before_retention(&intents);
                    let writes = ViewWrites::between(&previous, &source_views);
                    commit_views(&tx, source_id, &writes, &source_views.state)?;
                }
            }
        }
        if !writes.successors.is_empty() {
            let source = super::load_record(&tx, &mut self.verified_submissions, session_id)?;
            let intents = intents_of(&tx, &mut self.verified_submissions, session_id)?;
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
                    .map_err(|error| super::write_error(error, "product store write failed"))?;
                if exists {
                    let existing =
                        super::load_record(&tx, &mut self.verified_submissions, &target.id)?;
                    if existing.create_operation_id != target.create_operation_id
                        || existing.source_product_session_id != target.source_product_session_id
                        || existing.source_workflow_operation_id
                            != target.source_workflow_operation_id
                    {
                        return Err("successor identity is bound to another request"
                            .to_owned()
                            .into());
                    }
                    continue;
                }
                let claimed: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM identity_bindings WHERE identity=?1 AND kind='create')",
                        [&target.create_operation_id],
                        |row| row.get(0),
                    )
                    .map_err(|error| super::write_error(error, "product store write failed"))?;
                if claimed {
                    continue;
                }
                target.store_revision = super::commit_in_transaction(&tx, &target, 0, None)?;
                for notice in &intents.notices {
                    if notice.anchor < successor.published_seq.get() {
                        insert_notice(&tx, &target.id, notice)?;
                    }
                }
                let has_cursor: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM projector_cursors WHERE session_id=?1)",
                        [&target.id],
                        |row| row.get(0),
                    )
                    .map_err(|error| super::write_error(error, "product store write failed"))?;
                if let Some(seed) = &successor.seed
                    && !has_cursor
                {
                    let writes = ViewWrites::between(&SessionViews::empty(), seed);
                    // A pre-existing target cursor wins; never rewind it.
                    let _ = commit_views(&tx, &target.id, &writes, &seed.state)?;
                }
            }
        }
        match tx.commit() {
            Ok(()) => Ok(ProjectionCommit::Applied),
            Err(error) => projection_refusal(error),
        }
    }

    /// Deletes one session's views and cursor: always safe, the next
    /// projector run rebuilds them from the feed.
    pub fn delete_views(&mut self, session_id: &str) -> Result<(), RecordStoreError> {
        self.durability(false)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| super::write_error(error, "product store write failed"))?;
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
            .map_err(|error| super::write_error(error, "product store write failed"))?;
        }
        tx.commit()
            .map_err(|error| super::write_error(error, "product store write failed"))
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "View commit keeps session revision CAS, transcript replacement, queue publication, and transaction completion together."
)]
fn commit_views(
    tx: &Connection,
    session_id: &str,
    writes: &ViewWrites,
    views: &SessionViewState,
) -> Result<ProjectionCommit, RecordStoreError> {
    let runtime_session = views
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
        .map_err(|error| super::write_error(error, "product store write failed"))?;
    if moved != 1 {
        return Ok(ProjectionCommit::CursorMoved);
    }
    let state = serde_json::to_string(&views).map_err(|error| {
        super::write_error(error, "product session views are not encodable".to_owned())
    })?;
    let counters = views.counters;
    let recorded: Option<String> = tx
        .query_row(
            "SELECT locator FROM sessions WHERE id=?1",
            [session_id],
            |row| row.get(0),
        )
        .map_err(|error| super::write_error(error, "product store write failed"))?;
    let recorded = recorded
        .map(|locator| locator.parse::<SessionLocator>())
        .transpose()
        .map_err(|error| {
            super::write_error(error, "product session locator is invalid".to_owned())
        })?;
    let head = views.projected_locator(recorded);
    tx.execute(
            "INSERT INTO session_views(session_id,head_locator,root_turn_count,tool_call_count,compaction_count,state) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session_id) DO UPDATE SET head_locator=excluded.head_locator,root_turn_count=excluded.root_turn_count,tool_call_count=excluded.tool_call_count,compaction_count=excluded.compaction_count,state=excluded.state",
            params![
                session_id,
                head.map(|head| head.to_string()),
                to_i64(counters.root_turns)?,
                to_i64(counters.tool_calls)?,
                to_i64(counters.compactions)?,
                state,
            ],
        )
        .map_err(|error| super::write_error(error, "product store write failed"))?;
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
            .map_err(|error| super::write_error(error, "product store write failed"))?;
    }
    for (key, row) in &writes.upserts {
        if let Some(call_id) = row.call_id() {
            let duplicated: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM transcript_rows WHERE session_id=?1 AND call_id=?2 AND NOT (anchor=?3 AND lane=?4 AND ordinal=?5))",
                params![session_id, call_id, to_i64(key.anchor)?, key.lane.number(), key.ordinal],
                |row| row.get(0),
            ).map_err(|error| super::write_error(error, "product store write failed"))?;
            debug_assert!(!duplicated, "duplicated transcript call id: {call_id}");
        }
        let text = serde_json::to_string(row).map_err(|error| {
            super::write_error(error, "transcript row is not encodable".to_owned())
        })?;
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
            .map_err(|error| super::write_error(error, "could not write a transcript row".to_owned()))?;
    }
    Ok(ProjectionCommit::Applied)
}

fn write_keyed<K: rusqlite::ToSql, V: serde::Serialize>(
    connection: &Connection,
    session_id: &str,
    table: &str,
    key_column: &str,
    changes: &[(K, Option<V>)],
) -> Result<(), RecordStoreError> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut upsert = connection.prepare(&format!(
        "INSERT INTO {table}(session_id,{key_column},value) VALUES(?1,?2,?3) ON CONFLICT(session_id,{key_column}) DO UPDATE SET value=excluded.value"
    )).map_err(|error| super::write_error(error, "product store write failed"))?;
    let mut delete = connection
        .prepare(&format!(
            "DELETE FROM {table} WHERE session_id=?1 AND {key_column}=?2"
        ))
        .map_err(|error| super::write_error(error, "product store write failed"))?;
    for (key, value) in changes {
        if let Some(value) = value {
            let value = serde_json::to_string(value).map_err(|error| {
                super::write_error(error, "product keyed view is not encodable".to_owned())
            })?;
            upsert
                .execute(params![session_id, key, value])
                .map_err(|error| super::write_error(error, "product store write failed"))?;
        } else {
            delete
                .execute(params![session_id, key])
                .map_err(|error| super::write_error(error, "product store write failed"))?;
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
    verified: &mut VerifiedSubmissions,
    session_id: &str,
    submission: &Submission,
) -> Result<(), RecordStoreError> {
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
    .map_err(|error| super::write_error(error, "product store write failed"))?;
    let stored = load_submissions_where(tx, verified, "command_id=?1", &submission.command_id)?;
    if stored.first().map(|(owner, row)| (owner.as_str(), row)) != Some((session_id, submission)) {
        return Err(format!(
            "command id {} is already bound to another submission",
            submission.command_id
        )
        .into());
    }
    Ok(())
}

/// Records the host pause atomically with withdrawal of an unrecorded command.
pub(super) fn insert_pause_cause(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    cause: &HostPauseCause,
) -> Result<(), RecordStoreError> {
    let anchor: i64 = tx
        .query_row(
            "SELECT applied_seq FROM projector_cursors WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| super::write_error(error, "product store write failed"))?
        .unwrap_or(0);
    let ordinal: u32 = tx.query_row("SELECT COALESCE(MAX(ordinal)+1,0) FROM host_intents WHERE session_id=?1 AND kind=?2 AND anchor=?3", params![session_id, PAUSE_CAUSE, anchor], |row| row.get(0)).map_err(|error| super::write_error(error, "product store write failed"))?;
    let value = serde_json::to_string(cause)
        .map_err(|error| super::write_error(error, "pause cause is not encodable".to_owned()))?;
    tx.execute(
        "INSERT INTO host_intents(session_id,kind,anchor,ordinal,value) VALUES(?1,?2,?3,?4,?5)",
        params![session_id, PAUSE_CAUSE, anchor, ordinal, value],
    )
    .map_err(|error| super::write_error(error, "product store write failed"))?;
    Ok(())
}

/// The queue command join needs no transcript or per-call bookkeeping reads.
pub(super) fn queue_commands_of(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
    session_id: &str,
) -> Result<plakous_hosted::sessions::QueueCommands, String> {
    let intents = ProductIntents {
        submission_anchors: intent_anchors(connection, session_id, "submissions", "command_id")?,
        submissions: load_submissions_where(connection, verified, "session_id=?1", session_id)?
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
fn inherited_submissions_of(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
    id: &str,
) -> Result<Vec<Submission>, String> {
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
            load_submissions_where(connection, verified, "session_id=?1", &source_id)?
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
    verified: &mut VerifiedSubmissions,
    session_id: &str,
) -> Result<ProductIntents, String> {
    let submissions = load_submissions_where(connection, verified, "session_id=?1", session_id)?
        .into_iter()
        .map(|(_, submission)| submission)
        .collect();
    let mut statement = connection
        .prepare("SELECT operation_id,source_locator,kind,compact_reason,selection FROM workflow_requests WHERE session_id=?1 ORDER BY rowid")
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
            ROOT_INTERRUPT => intents.root_interrupts.push(AnchoredIntent {
                anchor,
                ordinal,
                value: RootInterrupt::decode(&value)?,
            }),
            ROOT_CONTINUATION => intents.root_continuations.push(AnchoredIntent {
                anchor,
                ordinal,
                value: RootContinuation::decode(&value)?,
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
/// carried-forward session's first commit first writes its imported
/// transcript at anchor 0, lane 1, in document order; the notices recorded
/// before that commit follow it as notices, under their own identities.
pub(super) fn write_notices(
    connection: &Connection,
    session: &plakous_hosted::sessions::SessionRecord,
    first_commit: bool,
) -> Result<(), RecordStoreError> {
    if first_commit && session.carried_forward_from().is_some() {
        write_import_intents(connection, &session.id, session.transcript.imported_rows())?;
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
        .map_err(|error| super::write_error(error, "product store write failed"))?
        .unwrap_or(0);
    let mut next: u32 = connection
        .query_row(
            "SELECT COALESCE(MAX(ordinal)+1,0) FROM operator_notices WHERE session_id=?1 AND anchor=?2",
            params![session.id, anchor],
            |row| row.get(0),
        )
        .map_err(|error| super::write_error(error, "product store write failed"))?;
    for (id, row) in session.transcript.pending_notice_entries() {
        if let Some(id) = id {
            let prior: Option<String> = connection
                .query_row(
                    "SELECT row FROM operator_notices WHERE session_id=?1 AND notice_id=?2",
                    params![session.id, id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|error| {
                    super::write_error(error, "could not read operator notice identity")
                })?;
            if let Some(prior) = prior {
                let prior: TranscriptRow = serde_json::from_str(&prior).map_err(|error| {
                    super::write_error(error, "operator notice is not decodable")
                })?;
                if prior != *row {
                    return Err("operator notice identity changed".to_owned().into());
                }
                continue;
            }
        }
        insert_notice(
            connection,
            &session.id,
            &OperatorNotice {
                notice_id: id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned),
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
    verified: &mut VerifiedSubmissions,
    session_id: &str,
) -> Result<
    (
        plakous_hosted::sessions::ProductTranscript,
        Option<SessionViews>,
        plakous_hosted::sessions::QueueCommands,
    ),
    String,
> {
    let intents = intents_of(connection, verified, session_id)?;
    let views = views_of(connection, session_id)?;
    let queue_commands =
        plakous_hosted::sessions::QueueCommands::from_join(&intents, views.as_ref());
    let rows = views
        .as_ref()
        .unwrap_or(&SessionViews::empty())
        .transcript(&intents);
    Ok((
        plakous_hosted::sessions::ProductTranscript::stored(rows),
        views,
        queue_commands,
    ))
}

pub(super) fn write_import_intents<'a>(
    connection: &Connection,
    session_id: &str,
    rows: impl IntoIterator<Item = &'a TranscriptRow>,
) -> Result<(), RecordStoreError> {
    for (ordinal, row) in rows.into_iter().enumerate() {
        let ordinal = u32::try_from(ordinal).map_err(|error| {
            super::write_error(error, "imported transcript is too long".to_owned())
        })?;
        insert_notice(
            connection,
            session_id,
            &OperatorNotice::imported(ordinal, row.clone()),
        )?;
    }
    Ok(())
}

fn workflow_intent_cas(
    tx: &Connection,
    session: &plakous_hosted::sessions::SessionRecord,
) -> Result<u64, RecordStoreError> {
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
        .map_err(|error| super::write_error(error, "product store write failed"))?;
    if changed != 1 {
        return Err("product session changed concurrently".to_owned().into());
    }
    Ok(next)
}

fn insert_notice(
    connection: &Connection,
    session_id: &str,
    notice: &OperatorNotice,
) -> Result<(), RecordStoreError> {
    let row = serde_json::to_string(&notice.row).map_err(|error| {
        super::write_error(error, "operator notice is not encodable".to_owned())
    })?;
    connection
        .execute(
            "INSERT INTO operator_notices(session_id,notice_id,anchor,ordinal,row) VALUES(?1,?2,?3,?4,?5)",
            params![session_id, notice.notice_id, to_i64(notice.anchor)?, notice.ordinal, row],
        )
        .map_err(|error| super::write_error(error, format!("could not record operator notice {}", notice.notice_id)))?;
    Ok(())
}

/// The submissions one store verified against their permanent identity
/// bindings (spec 02 §2.6), by owner session and command id, with the exact
/// immutable fields that matched. A command binding is never removed and never
/// changes once written, so a row equal to its verified fields needs no second
/// check through this store; a changed row misses and is checked again, and a
/// new process checks every row on its first load. It grows by one entry per
/// verified submission until a discard, a session delete or the end of the
/// connection.
#[derive(Debug, Default)]
pub(super) struct VerifiedSubmissions(HashMap<String, HashMap<String, Submission>>);

impl VerifiedSubmissions {
    fn contains(&self, owner: &str, submission: &Submission) -> bool {
        self.0
            .get(owner)
            .and_then(|verified| verified.get(&submission.command_id))
            == Some(submission)
    }

    /// Forgets a discarded submission.
    pub(super) fn forget(&mut self, owner: &str, command_id: &str) {
        if let Some(verified) = self.0.get_mut(owner) {
            verified.remove(command_id);
        }
    }

    /// Forgets every submission of a deleted session.
    pub(super) fn forget_session(&mut self, owner: &str) {
        self.0.remove(owner);
    }
}

/// Loads submissions and checks each against its permanent identity binding,
/// unless `verified` holds its exact fields. The unverified rows are checked in
/// one query. Only rows read outside a write transaction are remembered: a
/// write transaction's own rows and bindings are not durable until it commits.
fn load_submissions_where(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
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
    let unverified = submissions
        .iter()
        .filter(|(owner, submission)| !verified.contains(owner, submission))
        .collect::<Vec<_>>();
    if unverified.is_empty() {
        return Ok(submissions);
    }
    let identities = serde_json::to_string(
        &unverified
            .iter()
            .map(|(_, submission)| submission.binding_identity())
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
    for (owner, submission) in &unverified {
        let (bound_owner, digest) = bindings
            .get(&submission.binding_identity())
            .ok_or_else(|| "submission contradicts its permanent identity binding".to_owned())?;
        if bound_owner != owner || *digest != submission.binding_digest(owner) {
            return Err("submission contradicts its permanent identity binding".to_owned());
        }
    }
    if matches!(
        connection.transaction_state(None::<&str>),
        Ok(TransactionState::None | TransactionState::Read)
    ) {
        for (owner, submission) in unverified {
            verified
                .0
                .entry(owner.clone())
                .or_default()
                .insert(submission.command_id.clone(), submission.clone());
        }
    }
    Ok(submissions)
}

mod cursor_intents;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
