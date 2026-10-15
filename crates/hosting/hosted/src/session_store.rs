//! The port a hosted session persists its record through.
//!
//! The runtime owns the [`crate::sessions::SessionRecord`] as data and never
//! opens a database: it asks a [`SessionRecordStore`](crate::session_store::SessionRecordStore) to load, save, prepare
//! and publish. The product implements the port over its SQLite file; tests run
//! the same contract over an in-memory one.
//!
//! The record carries its CAS token (`store_revision`) as plain data; a store
//! keeps whatever else it needs to diff a commit.

use std::path::Path;

use crate::error::StoreRefusal;
use crate::sessions::SessionRecord;
use crate::sessions::projector::{
    ProductIntents, ProjectionCommit, SessionViews, Submission, ViewWrites,
};

/// A product-store failure. With a [`StoreRefusal`] it wrote nothing and the
/// same call may succeed when retried; without one it is final for the call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordStoreError {
    message: String,
    refusal: Option<StoreRefusal>,
}

impl RecordStoreError {
    /// A transient refusal: nothing was written.
    pub fn refused(refusal: StoreRefusal, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            refusal: Some(refusal),
        }
    }

    /// The transient refusal class, when the failure is one.
    #[must_use]
    pub const fn refusal(&self) -> Option<StoreRefusal> {
        self.refusal
    }

    /// Human-readable failure message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for RecordStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RecordStoreError {}

impl From<String> for RecordStoreError {
    fn from(message: String) -> Self {
        Self {
            message,
            refusal: None,
        }
    }
}

impl From<&str> for RecordStoreError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl From<RecordStoreError> for String {
    fn from(error: RecordStoreError) -> Self {
        error.message
    }
}

/// Durable product session records with compare-and-swap commits.
pub trait SessionRecordStore: Send + Sync {
    /// Load one record by its exact id. Queue items and unresolved delivery or
    /// workflow journals project as paused; loading never writes that projection
    /// or advances the durable revision.
    ///
    /// # Errors
    /// `session not found`, or a message when the row is invalid.
    fn load(&self, id: &str) -> Result<SessionRecord, String>;

    /// Commit `session` at its `store_revision` and advance the token.
    ///
    /// # Errors
    /// A concurrent commit, an identity conflict, or an invalid record.
    fn save(&self, session: &mut SessionRecord) -> Result<(), String>;

    /// Writes host decisions at the current applied cursor. Runtime outcomes
    /// are never recorded here: the joined views derive them from facts.
    ///
    /// # Errors
    /// A missing session, exhausted ordinal, or store failure.
    fn write_host_controls(
        &self,
        id: &str,
        suppress_wake: bool,
        pause: Option<crate::sessions::projector::HostPauseCause>,
    ) -> Result<(), String>;

    /// Commits an immutable command submission and its queue item together,
    /// under the record's revision CAS, before dispatch.
    ///
    /// # Errors
    /// A concurrent commit, an identity conflict, or a store failure; a store
    /// that another writer holds refuses with [`StoreRefusal::Busy`].
    fn save_submission(
        &self,
        session: &mut SessionRecord,
        submission: &Submission,
    ) -> Result<(), RecordStoreError>;

    /// Removes an unrecorded submission together with the record changes,
    /// under the record's revision CAS, and writes a pause cause when no
    /// runtime fact carries a session-wide refusal.
    ///
    /// # Errors
    /// A concurrent commit or a store failure.
    fn discard_submission(
        &self,
        session: &mut SessionRecord,
        command_id: &str,
        pause_cause: Option<crate::sessions::projector::HostPauseCause>,
    ) -> Result<(), String>;

    /// Writes an immutable workflow request before its runtime submission,
    /// under the session revision CAS. No workflow outcome is stored here.
    ///
    /// # Errors
    /// A stale revision, an identity conflict, or a store refusal.
    fn submit_workflow_request(
        &self,
        session: &mut SessionRecord,
        request: &crate::sessions::projector::WorkflowRequest,
    ) -> Result<(), String>;

    /// Removes only this request after a definite synchronous answer without
    /// admission. Call after projecting to the feed tail. The identity binding
    /// is terminalized; no recovery/retry notice is authored for a live answer.
    ///
    /// # Errors
    /// A stale revision, missing views, a recorded fact, or a store refusal.
    fn discard_unadmitted_workflow(
        &self,
        session: &mut SessionRecord,
        operation_id: &str,
    ) -> Result<(), String>;

    /// Withdraws requests without a feed fact and writes one notice for each,
    /// atomically under the session revision CAS. Call only after exact open
    /// and a successful projector pass to the source and successor feed tails.
    /// That pass proves a withdrawn clear's reserved genesis has no runtime
    /// session; withdrawal removes only its empty, view-less reservation if it
    /// still exists (a deleted reservation needs no join),
    /// retaining permanent identity bindings.
    ///
    /// # Errors
    /// A stale revision, missing views, or a store refusal.
    fn withdraw_unadmitted_workflows(
        &self,
        session: &mut SessionRecord,
    ) -> Result<Vec<crate::sessions::TranscriptRow>, String>;

    /// Records rooted at `workspace`, newest first, as listing projections.
    ///
    /// # Errors
    /// A message when the store cannot be read.
    fn list_for_workspace(&self, workspace: &Path) -> Result<Vec<SessionRecord>, String>;

    /// Every intent of one session the projector reads (spec 02 §2.6).
    ///
    /// # Errors
    /// A message when the store cannot be read or a row is invalid.
    fn load_intents(&self, id: &str) -> Result<ProductIntents, String>;

    /// The immutable submissions of same-runtime ancestors. Reads only ancestry
    /// metadata and submissions, not full records.
    ///
    /// # Errors
    /// A missing ancestor, cyclic ancestry or invalid submission.
    fn load_inherited_submissions(&self, id: &str) -> Result<Vec<Submission>, String>;

    /// The stored views of one session, or `None` before its first projector
    /// transaction or after its views were deleted for a rebuild.
    ///
    /// # Errors
    /// A message when the store cannot be read or a row is invalid.
    fn load_views(&self, id: &str) -> Result<Option<SessionViews>, String>;

    /// Commits one projector run under the cursor CAS: when the stored cursor
    /// is still `writes.expected_cursor`, writes `writes` and `views`' state
    /// and advances the cursor; otherwise changes nothing.
    ///
    /// # Errors
    /// A store failure; nothing is written.
    fn commit_projection_views(
        &self,
        id: &str,
        writes: &ViewWrites,
        views: &SessionViews,
    ) -> Result<ProjectionCommit, String>;

    /// Emits one batch of disposable usage observations after the winning commit.
    /// A crash between commit and emission loses the observation, never authority.
    fn emit_projected_turn_usages(&self, usages: &[crate::sessions::projector::ProjectedTurnUsage]);
}

#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryStore;

#[cfg(any(test, feature = "test-support"))]
mod memory {
    use std::collections::{BTreeMap, HashMap};
    use std::path::Path;
    use std::sync::Mutex;

    use super::{RecordStoreError, SessionRecordStore};
    use crate::sessions::projector::{
        ProductIntents, ProjectionCommit, SessionViews, Submission, ViewWrites,
    };
    use crate::sessions::{SessionRecord, SessionWorkflowKind, digest_parts, workspace_identity};

    /// One stable product identity the store has bound: who owns it, the
    /// digest of the exact request it names, and its terminal disposition once
    /// the receipt compacted away.
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Binding {
        kind: &'static str,
        owner: String,
        digest: String,
        terminal: Option<(String, Option<String>)>,
    }

    #[derive(Clone, Debug, Default)]
    struct State {
        records: HashMap<String, SessionRecord>,
        bindings: BTreeMap<String, Binding>,
        intents: HashMap<String, ProductIntents>,
        views: HashMap<String, SessionViews>,
        usage_emissions: Vec<crate::sessions::projector::ProjectedTurnUsage>,
    }

    /// An in-memory [`SessionRecordStore`] with the SQLite store's contract:
    /// CAS commits, stable identity bindings with compacted terminal
    /// dispositions, and atomic branch publication.
    #[derive(Debug, Default)]
    pub struct MemoryStore {
        state: Mutex<State>,
        #[cfg(test)]
        view_reads: std::sync::atomic::AtomicUsize,
        #[cfg(test)]
        record_reads: std::sync::atomic::AtomicUsize,
        #[cfg(test)]
        intent_reads: std::sync::atomic::AtomicUsize,
        #[cfg(test)]
        inherited_reads: std::sync::atomic::AtomicUsize,
    }

    impl MemoryStore {
        /// Terminal observations emitted by winning projector transactions.
        pub fn projected_usage_emissions(
            &self,
        ) -> Vec<crate::sessions::projector::ProjectedTurnUsage> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .usage_emissions
                .clone()
        }
        /// An empty store.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        #[cfg(test)]
        pub(crate) fn view_read_count(&self) -> usize {
            self.view_reads.load(std::sync::atomic::Ordering::Relaxed)
        }

        #[cfg(test)]
        pub(crate) fn record_read_count(&self) -> usize {
            self.record_reads.load(std::sync::atomic::Ordering::Relaxed)
        }

        #[cfg(test)]
        pub(crate) fn intent_read_count(&self) -> usize {
            self.intent_reads.load(std::sync::atomic::Ordering::Relaxed)
        }

        #[cfg(test)]
        pub(crate) fn inherited_read_count(&self) -> usize {
            self.inherited_reads
                .load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Deletes one session's views and cursor, as a rebuild does.
        pub fn delete_views(&self, id: &str) {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .views
                .remove(id);
        }

        /// Replaces one session's intents.
        pub fn set_intents(&self, id: &str, intents: ProductIntents) {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .intents
                .insert(id.to_owned(), intents);
        }

        /// Every record the store holds, by id.
        #[must_use]
        pub fn records(&self) -> Vec<SessionRecord> {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .records
                .values()
                .cloned()
                .map(|record| loaded(&state, record))
                .collect()
        }
    }

    fn bind(
        bindings: &mut BTreeMap<String, Binding>,
        kind: &'static str,
        identity: &str,
        owner: &str,
        digest: String,
        terminal: Option<(String, Option<String>)>,
    ) -> Result<(), String> {
        let contradiction = "stable product identity contradicts durable state".to_owned();
        match bindings.get_mut(identity) {
            None => {
                bindings.insert(
                    identity.to_owned(),
                    Binding {
                        kind,
                        owner: owner.to_owned(),
                        digest,
                        terminal,
                    },
                );
                Ok(())
            }
            Some(existing) => {
                if existing.kind != kind || existing.owner != owner || existing.digest != digest {
                    return Err(contradiction);
                }
                match (&existing.terminal, terminal) {
                    (Some(_), None) => Err(contradiction),
                    (Some(current), Some(next)) if *current != next => Err(contradiction),
                    (_, next) => {
                        if next.is_some() {
                            existing.terminal = next;
                        }
                        Ok(())
                    }
                }
            }
        }
    }

    fn sync_queue(
        bindings: &mut BTreeMap<String, Binding>,
        previous: Option<&SessionRecord>,
        session: &SessionRecord,
    ) -> Result<(), String> {
        for item in &session.pending_queue {
            bind(
                bindings,
                "queue",
                &item.id,
                &session.id,
                session.id.clone(),
                None,
            )?;
            if let Some(old) = previous
                .and_then(|record| record.pending_queue.iter().find(|old| old.id == item.id))
                && !((old.revision == item.revision && old.text == item.text)
                    || old.revision.checked_add(1) == Some(item.revision))
            {
                return Err("queue identity contradicts durable product state".to_owned());
            }
        }
        let Some(previous) = previous else {
            return Ok(());
        };
        for item in previous
            .pending_queue
            .iter()
            .filter(|item| !session.pending_queue.iter().any(|next| next.id == item.id))
        {
            let binding = bindings
                .get_mut(&item.id)
                .filter(|binding| {
                    binding.kind == "queue"
                        && binding.owner == session.id
                        && binding.digest == session.id
                        && binding.terminal.is_none()
                })
                .ok_or_else(|| "queue tombstone contradicts durable state".to_owned())?;
            binding.digest = digest_parts(&[
                &session.id,
                &item.revision.to_string(),
                &digest_parts(&[&item.text]),
            ]);
            binding.terminal = Some(("deleted".to_owned(), None));
        }
        Ok(())
    }

    /// Records `session`'s operator notices as intents at the cursor the store
    /// applied, as the SQLite store does in the record's transaction; a
    /// carried-forward session's first commit records its imported rows.
    fn persist_notices(state: &mut State, session: &SessionRecord, first_commit: bool) {
        let anchor = state
            .views
            .get(&session.id)
            .map_or(0, |views| views.cursor.get());
        let intents = state.intents.entry(session.id.clone()).or_default();
        if first_commit && session.carried_forward_from().is_some() {
            intents.imported = session
                .transcript
                .iter()
                .filter(|row| !matches!(row, crate::sessions::TranscriptRow::Truncated))
                .cloned()
                .collect();
            return;
        }
        let mut ordinal = intents
            .notices
            .iter()
            .filter(|notice| notice.anchor == anchor)
            .map(|notice| notice.ordinal.saturating_add(1))
            .max()
            .unwrap_or(0);
        for row in session.transcript.pending_notices() {
            intents
                .notices
                .push(crate::sessions::projector::OperatorNotice {
                    notice_id: format!("memory-notice-{}", intents.notices.len()),
                    anchor,
                    ordinal,
                    row: row.clone(),
                });
            ordinal = ordinal.saturating_add(1);
        }
    }

    fn inherited_submissions(state: &State, id: &str) -> Result<Vec<Submission>, String> {
        let record = state
            .records
            .get(id)
            .ok_or_else(|| "session not found".to_owned())?;
        let runtime = record
            .plakous_locator
            .map(plakous::sessions::SessionLocator::session_id);
        let mut record = record;
        let mut seen = std::collections::BTreeSet::from([id.to_owned()]);
        let mut submissions = Vec::new();
        while record.carried_forward_from().is_none() {
            let Some(source) = &record.source_product_session_id else {
                break;
            };
            if !seen.insert(source.clone()) {
                return Err("projection intent ancestry is cyclic".to_owned());
            }
            record = state
                .records
                .get(source)
                .ok_or_else(|| "session not found".to_owned())?;
            if runtime.is_none()
                || record
                    .plakous_locator
                    .map(plakous::sessions::SessionLocator::session_id)
                    != runtime
            {
                break;
            }
            if let Some(intents) = state.intents.get(source) {
                submissions.extend(intents.submissions.iter().cloned());
            }
        }
        Ok(submissions)
    }

    /// A stored record as a load returns it: its transcript and counters from
    /// the views (spec 02 §2.6).
    fn loaded(state: &State, mut record: SessionRecord) -> SessionRecord {
        record.queue_projection_debt = false;
        record.queue_drain_state = crate::sessions::QueueDrainState::LiveAutoDrain;
        let intents = state.intents.get(&record.id).cloned().unwrap_or_default();
        let views = state.views.get(&record.id);
        if let Some(head) = views.and_then(|views| views.state.head) {
            record.plakous_locator = Some(plakous::sessions::SessionLocator::new(
                head.session_id,
                head.branch_id,
                head.checkpoint_id,
            ));
        }
        record.queue_commands = crate::sessions::queue::QueueCommands::from_join(&intents, views);
        record
            .pending_queue
            .retain(|item| !record.queue_commands.is_consumed(item));
        record.transcript = crate::sessions::ProductTranscript::committed(
            views
                .unwrap_or(&SessionViews::empty())
                .transcript(&intents)
                .into_iter()
                .map(|(_, row)| row)
                .collect(),
        );
        let empty = SessionViews::empty();
        let joined = views.unwrap_or(&empty);
        record.child_wake_suppressed = joined.wake_suppressed(&intents);
        if joined.pause_cause(&intents).is_some() {
            record.queue_drain_state = crate::sessions::QueueDrainState::RestoredPaused;
        }
        // Spec 02 §2.6: the persisted counters are the views' only.
        let counters = views.map(|views| views.state.counters).unwrap_or_default();
        record.root_turn_count = counters.root_turns;
        record.tool_call_count = counters.tool_calls;
        record.compaction_count = counters.compactions;
        record
    }

    fn commit_locked(state: &mut State, session: &mut SessionRecord) -> Result<(), String> {
        session.validate_for_store()?;
        let expected = session.store_revision;
        let current = state.records.get(&session.id);
        let next = match (expected, current) {
            (0, None) => 1,
            (0, Some(_)) => {
                return Err("product session identity already exists or is invalid".to_owned());
            }
            (expected, Some(current))
                if expected == current.store_revision
                    && session.create_operation_id == current.create_operation_id
                    && session.created_at_unix == current.created_at_unix =>
            {
                expected
                    .checked_add(1)
                    .ok_or_else(|| "product session revision is exhausted".to_owned())?
            }
            _ => return Err("product session changed concurrently".to_owned()),
        };
        // Validate every identity in a private registry before publishing either
        // the registry or the caller's revision. A later collision must not
        // reserve identities from a save/prepare that never committed.
        let mut bindings = state.bindings.clone();
        if expected == 0 {
            bind(
                &mut bindings,
                "create",
                &session.create_operation_id,
                &session.id,
                session.id.clone(),
                None,
            )?;
        }
        sync_queue(&mut bindings, current, session)?;
        persist_notices(state, session, expected == 0);
        session.transcript.mark_committed();
        let mut stored = session.clone();
        stored.store_revision = next;
        stored.queue_drain_state = crate::sessions::QueueDrainState::LiveAutoDrain;
        stored.child_wake_suppressed = false;
        state.bindings = bindings;
        session.store_revision = next;
        state.records.insert(session.id.clone(), stored);
        session.queue_commands = crate::sessions::queue::QueueCommands::from_join(
            state
                .intents
                .get(&session.id)
                .unwrap_or(&ProductIntents::default()),
            state.views.get(&session.id),
        );
        Ok(())
    }

    fn insert_submission(
        state: &mut State,
        id: &str,
        submission: &Submission,
    ) -> Result<(), String> {
        bind(
            &mut state.bindings,
            "command",
            &submission.binding_identity(),
            id,
            submission.binding_digest(id),
            None,
        )?;
        for (owner, intents) in &state.intents {
            if let Some(existing) = intents.submission(&submission.command_id) {
                return if owner == id && existing == submission {
                    Ok(())
                } else {
                    Err("submission command id is already bound to another request".to_owned())
                };
            }
        }
        let anchor = state.views.get(id).map_or(0, |views| views.cursor.get());
        let intents = state.intents.entry(id.to_owned()).or_default();
        intents
            .submission_anchors
            .insert(submission.command_id.clone(), anchor);
        intents.submissions.push(submission.clone());
        Ok(())
    }

    impl SessionRecordStore for MemoryStore {
        fn emit_projected_turn_usages(
            &self,
            usages: &[crate::sessions::projector::ProjectedTurnUsage],
        ) {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .usage_emissions
                .extend_from_slice(usages);
        }
        fn write_host_controls(
            &self,
            id: &str,
            suppress_wake: bool,
            pause: Option<crate::sessions::projector::HostPauseCause>,
        ) -> Result<(), String> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.records.contains_key(id) {
                return Err("session not found".to_owned());
            }
            let anchor = state.views.get(id).map_or(0, |views| views.cursor.get());
            let intents = state.intents.entry(id.to_owned()).or_default();
            let ordinal = intents
                .wake_suppressions
                .iter()
                .map(|intent| (intent.anchor, intent.ordinal))
                .chain(
                    intents
                        .pause_causes
                        .iter()
                        .map(|intent| (intent.anchor, intent.ordinal)),
                )
                .filter(|(at, _)| *at == anchor)
                .map(|(_, ordinal)| ordinal)
                .max()
                .map_or(Some(0), |ordinal| ordinal.checked_add(1))
                .ok_or_else(|| "host intent ordinal is exhausted".to_owned())?;
            if suppress_wake {
                intents
                    .wake_suppressions
                    .push(crate::sessions::projector::AnchoredIntent {
                        anchor,
                        ordinal,
                        value: (),
                    });
            }
            if let Some(value) = pause {
                intents
                    .pause_causes
                    .push(crate::sessions::projector::AnchoredIntent {
                        anchor,
                        ordinal,
                        value,
                    });
            }
            Ok(())
        }

        fn load(&self, id: &str) -> Result<SessionRecord, String> {
            #[cfg(test)]
            self.record_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .records
                .get(id)
                .cloned()
                .map(|record| {
                    let mut record = loaded(&state, record);
                    record.pause_restored_work();
                    record
                })
                .ok_or_else(|| "session not found".to_owned())
        }

        fn save(&self, session: &mut SessionRecord) -> Result<(), String> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            commit_locked(&mut state, session)
        }

        fn save_submission(
            &self,
            session: &mut SessionRecord,
            submission: &Submission,
        ) -> Result<(), RecordStoreError> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut next = state.clone();
            let mut candidate = session.clone();
            insert_submission(&mut next, &session.id, submission)?;
            commit_locked(&mut next, &mut candidate)?;
            *state = next;
            candidate.queue_commands = crate::sessions::queue::QueueCommands::from_join(
                state
                    .intents
                    .get(&session.id)
                    .unwrap_or(&ProductIntents::default()),
                state.views.get(&session.id),
            );
            *session = candidate;
            Ok(())
        }

        fn discard_submission(
            &self,
            session: &mut SessionRecord,
            command_id: &str,
            pause_cause: Option<crate::sessions::projector::HostPauseCause>,
        ) -> Result<(), String> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut next = state.clone();
            let mut candidate = session.clone();
            commit_locked(&mut next, &mut candidate)?;
            if let Some(intents) = next.intents.get_mut(&session.id) {
                intents.submission_anchors.remove(command_id);
                intents
                    .submissions
                    .retain(|submission| submission.command_id != command_id);
            }
            if let Some(cause) = pause_cause {
                let anchor = next
                    .views
                    .get(&session.id)
                    .map_or(0, |views| views.cursor.get());
                let intents = next.intents.entry(session.id.clone()).or_default();
                let ordinal = intents
                    .pause_causes
                    .iter()
                    .filter(|intent| intent.anchor == anchor)
                    .map(|intent| intent.ordinal)
                    .max()
                    .map_or(Some(0), |ordinal| ordinal.checked_add(1))
                    .ok_or_else(|| "pause cause ordinal is exhausted".to_owned())?;
                intents
                    .pause_causes
                    .push(crate::sessions::projector::AnchoredIntent {
                        anchor,
                        ordinal,
                        value: cause,
                    });
            }
            *state = next;
            candidate.queue_commands = crate::sessions::queue::QueueCommands::from_join(
                state
                    .intents
                    .get(&session.id)
                    .unwrap_or(&ProductIntents::default()),
                state.views.get(&session.id),
            );
            *session = candidate;
            Ok(())
        }

        fn submit_workflow_request(
            &self,
            session: &mut SessionRecord,
            request: &crate::sessions::projector::WorkflowRequest,
        ) -> Result<(), String> {
            plakous::sessions::SessionOperationId::new(&request.operation_id)
                .map_err(|_| "workflow operation identity is invalid".to_owned())?;
            if session.plakous_locator != Some(request.source_locator) {
                return Err("workflow source is not the exact product binding".to_owned());
            }
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.intents.iter().any(|(id, intents)| {
                intents
                    .workflow_request(&request.operation_id)
                    .is_some_and(|existing| id != &session.id || existing != request)
            }) {
                return Err("workflow identity is bound to another request".to_owned());
            }
            let digest = request.binding_digest(&session.id);
            if state
                .bindings
                .get(&request.operation_id)
                .is_some_and(|binding| {
                    binding.kind != "workflow"
                        || binding.owner != session.id
                        || binding.digest != digest
                        || binding.terminal.is_some()
                })
            {
                return Err("workflow identity is bound to another request".to_owned());
            }
            let mut next = state.clone();
            let mut candidate = session.clone();
            candidate.touch();
            commit_locked(&mut next, &mut candidate)?;
            bind(
                &mut next.bindings,
                "workflow",
                &request.operation_id,
                &session.id,
                digest,
                None,
            )?;
            let anchor = next
                .views
                .get(&session.id)
                .map_or(0, |views| views.cursor.get());
            let intents = next.intents.entry(session.id.clone()).or_default();
            if intents.workflow_request(&request.operation_id).is_none() {
                intents
                    .workflow_anchors
                    .insert(request.operation_id.clone(), anchor);
                intents.workflow_requests.push(request.clone());
            }
            if request.kind == SessionWorkflowKind::Clear {
                let mut target = candidate.clear_successor_from_request(request)?;
                if let Some(existing) = next.records.get(&target.id) {
                    if existing.create_operation_id != target.create_operation_id
                        || existing.source_product_session_id != target.source_product_session_id
                        || existing.source_workflow_operation_id
                            != target.source_workflow_operation_id
                    {
                        return Err(
                            "clear successor identity is bound to another request".to_owned()
                        );
                    }
                } else {
                    commit_locked(&mut next, &mut target)?;
                }
            }
            *state = next;
            *session = candidate;
            Ok(())
        }

        fn discard_unadmitted_workflow(
            &self,
            session: &mut SessionRecord,
            operation_id: &str,
        ) -> Result<(), String> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let views = state
                .views
                .get(&session.id)
                .ok_or_else(|| "workflow discard requires projected views".to_owned())?;
            let intents = state.intents.get(&session.id).cloned().unwrap_or_default();
            let Some(request) = intents.workflow_request(operation_id) else {
                return Ok(());
            };
            if !crate::sessions::projector::pending_workflow_operation_ids(views, &intents)
                .iter()
                .any(|id| id == operation_id)
            {
                return Err("a recorded workflow request cannot be discarded".to_owned());
            }
            let mut candidate = session.clone();
            candidate.touch();
            let mut staged = state.clone();
            commit_locked(&mut staged, &mut candidate)?;
            bind(
                &mut staged.bindings,
                "workflow",
                operation_id,
                &session.id,
                request.binding_digest(&session.id),
                Some(("discarded".to_owned(), None)),
            )?;
            staged
                .intents
                .entry(session.id.clone())
                .or_default()
                .workflow_anchors
                .remove(operation_id);
            staged
                .intents
                .entry(session.id.clone())
                .or_default()
                .workflow_requests
                .retain(|request| request.operation_id != operation_id);
            *state = staged;
            *session = candidate;
            Ok(())
        }

        fn withdraw_unadmitted_workflows(
            &self,
            session: &mut SessionRecord,
        ) -> Result<Vec<crate::sessions::TranscriptRow>, String> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let views = state
                .views
                .get(&session.id)
                .ok_or_else(|| "workflow withdrawal requires projected views".to_owned())?;
            let intents = state.intents.get(&session.id).cloned().unwrap_or_default();
            let withdrawals = crate::sessions::projector::workflow_withdrawals(views, &intents);
            if withdrawals.is_empty() {
                return Ok(Vec::new());
            }
            let mut next = state.clone();
            let mut candidate = session.clone();
            candidate.touch();
            commit_locked(&mut next, &mut candidate)?;
            let mut rows = Vec::new();
            for (id, notice) in withdrawals {
                if let Some(request) = intents.workflow_request(&id)
                    && request.kind == SessionWorkflowKind::Clear
                {
                    let target_id = request.successor_product_id();
                    if let Some(target) = next.records.get(&target_id) {
                        if next.views.contains_key(&target_id)
                            || target.source_product_session_id.as_deref()
                                != Some(session.id.as_str())
                            || target.source_workflow_operation_id.as_deref() != Some(id.as_str())
                            || !target.prompt_history.is_empty()
                            || !target.pending_queue.is_empty()
                            || next.intents.get(&target_id).cloned().unwrap_or_default()
                                != ProductIntents::default()
                        {
                            return Err(
                                "unadmitted clear successor is not an empty reservation".to_owned()
                            );
                        }
                        next.records.remove(&target_id);
                        next.intents.remove(&target_id);
                    }
                }
                let intents = next.intents.entry(session.id.clone()).or_default();
                intents.workflow_anchors.remove(&id);
                intents
                    .workflow_requests
                    .retain(|request| request.operation_id != id);
                rows.push(notice.row.clone());
                intents.notices.push(notice);
            }
            *state = next;
            *session = candidate;
            Ok(rows)
        }

        fn list_for_workspace(&self, workspace: &Path) -> Result<Vec<SessionRecord>, String> {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let identity = workspace_identity(workspace);
            let mut records = state
                .records
                .values()
                .filter(|record| workspace_identity(&record.workspace_root) == identity)
                .cloned()
                .map(|mut record| {
                    record.store_revision = 0;
                    record.first_prompt = record.prompt_history.first().cloned();
                    record.transcript = crate::sessions::ProductTranscript::default();
                    record.prompt_history.clear();
                    record.pending_queue.clear();
                    record
                })
                .collect::<Vec<_>>();
            records.sort_by(|a, b| {
                b.updated_at_unix
                    .cmp(&a.updated_at_unix)
                    .then_with(|| a.id.cmp(&b.id))
            });
            Ok(records)
        }

        fn load_intents(&self, id: &str) -> Result<ProductIntents, String> {
            #[cfg(test)]
            self.intent_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .intents
                .get(id)
                .cloned()
                .unwrap_or_default())
        }

        fn load_views(&self, id: &str) -> Result<Option<SessionViews>, String> {
            #[cfg(test)]
            self.view_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .views
                .get(id)
                .cloned())
        }

        fn load_inherited_submissions(&self, id: &str) -> Result<Vec<Submission>, String> {
            #[cfg(test)]
            self.inherited_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            inherited_submissions(
                &self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                id,
            )
        }

        fn commit_projection_views(
            &self,
            id: &str,
            writes: &ViewWrites,
            views: &SessionViews,
        ) -> Result<ProjectionCommit, String> {
            #[cfg(test)]
            crate::runtime::observe_projector_view_write(id);
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The product store's views reference their session row.
            if !state.records.contains_key(id) {
                return Err("product session is not stored".to_owned());
            }
            let stored = state
                .views
                .get(id)
                .map_or(plakous::sessions::feed::FeedSeq::ZERO, |views| views.cursor);
            if stored != writes.expected_cursor {
                return Ok(ProjectionCommit::CursorMoved);
            }
            let mut next = state.clone();
            let mut joined = views.clone();
            for request in next
                .intents
                .get(id)
                .cloned()
                .unwrap_or_default()
                .workflow_requests
            {
                if request.kind != SessionWorkflowKind::Clear {
                    continue;
                }
                let target_id = request.successor_product_id();
                if let (Some(target), Some(target_views)) =
                    (next.records.get(&target_id), next.views.get(&target_id))
                {
                    if let Some(completion) =
                        crate::sessions::projector::clear_completion(&request, target, target_views)
                    {
                        joined
                            .state
                            .workflows
                            .insert(request.operation_id, completion);
                    }
                }
            }
            joined.consume_before_retention(&next.intents.get(id).cloned().unwrap_or_default());
            let views = &joined;
            let source = next
                .records
                .get(id)
                .cloned()
                .ok_or_else(|| "product session is not stored".to_owned())?;
            if let (Some(source_id), Some(operation)) = (
                source.source_product_session_id.as_deref(),
                source.source_workflow_operation_id.as_deref(),
            ) {
                let intents = next.intents.get(source_id).cloned().unwrap_or_default();
                if let Some(completion) = intents.workflow_request(operation).and_then(|request| {
                    crate::sessions::projector::clear_completion(request, &source, views)
                }) {
                    let source_views = next
                        .views
                        .entry(source_id.to_owned())
                        .or_insert_with(SessionViews::empty);
                    source_views
                        .state
                        .workflows
                        .insert(operation.to_owned(), completion);
                    source_views.consume_before_retention(&intents);
                }
            }
            let intents = next.intents.get(id).cloned().unwrap_or_default();
            for successor in &writes.successors {
                let Some(request) = successor
                    .operation_id
                    .as_deref()
                    .and_then(|id| intents.workflow_request(id))
                else {
                    continue;
                };
                let mut target = source.successor_from_request(request, successor)?;
                if let Some(existing) = next.records.get(&target.id) {
                    if existing.create_operation_id != target.create_operation_id
                        || existing.source_product_session_id != target.source_product_session_id
                        || existing.source_workflow_operation_id
                            != target.source_workflow_operation_id
                    {
                        return Err("successor identity is bound to another request".to_owned());
                    }
                    continue;
                }
                if next
                    .bindings
                    .get(&target.create_operation_id)
                    .is_some_and(|binding| binding.kind == "create")
                {
                    continue;
                }
                commit_locked(&mut next, &mut target)?;
                next.intents.entry(target.id.clone()).or_default().notices = intents
                    .notices
                    .iter()
                    .filter(|notice| notice.anchor < successor.published_seq.get())
                    .cloned()
                    .collect();
                next.intents.entry(target.id.clone()).or_default().imported =
                    intents.imported.clone();
            }
            next.views.insert(id.to_owned(), views.clone());
            *state = next;
            Ok(ProjectionCommit::Applied)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::sessions::projector::{Successor, WorkflowRequest};
        use plakous::sessions::feed::{FeedBranchTransition, FeedLocator, FeedSeq};

        #[test]
        fn a_deleted_clear_reservation_still_withdraws_once_in_memory() {
            let store = MemoryStore::new();
            let mut source = SessionRecord::new("/tmp/plak-memory-clear-deletion".into());
            let locator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003".parse().unwrap();
            source.bind_plakous_locator(locator).unwrap();
            store.save(&mut source).unwrap();
            let request = WorkflowRequest {
                operation_id: "plak:clear:deleted-reservation".into(),
                source_locator: locator,
                kind: SessionWorkflowKind::Clear,
            };
            store
                .submit_workflow_request(&mut source, &request)
                .unwrap();
            {
                let mut state = store.state.lock().unwrap();
                state.records.remove(&request.successor_product_id());
                state.intents.remove(&request.successor_product_id());
            }
            let empty = SessionViews::empty();
            store
                .commit_projection_views(&source.id, &ViewWrites::between(&empty, &empty), &empty)
                .unwrap();
            assert_eq!(
                store
                    .withdraw_unadmitted_workflows(&mut source)
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                store.withdraw_unadmitted_workflows(&mut source).unwrap(),
                [] as [plakous_host_protocol::views::transcript::TranscriptRow; 0]
            );
            let intents = store.load_intents(&source.id).unwrap();
            assert_eq!(intents.workflow_requests, [] as [WorkflowRequest; 0]);
            assert_eq!(intents.notices.len(), 1);
            assert!(store.load(&request.successor_product_id()).is_err());
        }

        #[test]
        fn deleted_successor_is_not_resurrected_by_memory_projection() {
            let store = MemoryStore::new();
            let mut source = SessionRecord::new("/tmp/plak-memory-successor".into());
            let locator: plakous::sessions::SessionLocator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003".parse().unwrap();
            source.bind_plakous_locator(locator).unwrap();
            store.save(&mut source).unwrap();
            let request = WorkflowRequest {
                operation_id: "plak:fork:memory-deletion".into(),
                source_locator: locator,
                kind: SessionWorkflowKind::Fork,
            };
            store
                .submit_workflow_request(&mut source, &request)
                .unwrap();
            let mut views = SessionViews::empty();
            views.cursor = FeedSeq::new(1);
            let mut target_views = views.clone();
            target_views.state.head = Some(FeedLocator {
                session_id: locator.session_id(),
                branch_id: "00000000-0000-4000-8000-000000000004".parse().unwrap(),
                checkpoint_id: locator.checkpoint_id(),
            });
            let mut writes = ViewWrites::between(&SessionViews::empty(), &views);
            writes.successors.push(Successor {
                operation_id: Some(request.operation_id.clone()),
                transition: FeedBranchTransition::Fork,
                target: target_views.state.head.unwrap(),
                published_seq: views.cursor,
                created_at_unix: 42,
            });
            store
                .commit_projection_views(&source.id, &writes, &views)
                .unwrap();
            let target = request.successor_product_id();
            assert_eq!(store.load(&target).unwrap().created_at_unix, 42);
            assert!(store.load_views(&target).unwrap().is_none());
            {
                let mut state = store.state.lock().unwrap();
                state.records.remove(&target);
                state.views.remove(&target);
                state.intents.remove(&target);
                state.views.remove(&source.id);
            }
            assert_eq!(
                store
                    .commit_projection_views(&source.id, &writes, &views)
                    .unwrap(),
                ProjectionCommit::Applied
            );
            assert!(
                store.load(&target).is_err(),
                "deleted successor resurrected"
            );
            assert!(store.load_views(&target).unwrap().is_none());
        }
    }
}
