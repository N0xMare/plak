//! The product's SQLite implementation of the hosted session-record port.
//!
//! The port itself is `plakous_hosted::session_store`, re-exported here so a
//! consumer's `crate::store::session_records::…` path does not change twice.

use std::path::Path;
use std::sync::{Arc, Mutex};

use plakous_hosted::sessions::projector::{
    ProductIntents, ProjectionCommit, SessionViews, ViewWrites,
};

#[cfg(any(test, feature = "test-support"))]
pub use plakous_hosted::session_store::MemoryStore;
pub use plakous_hosted::session_store::SessionRecordStore;

use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;

/// One lazily opened product database shared by command and hosted workers.
#[derive(Clone, Debug)]
pub struct SqliteSessionStore {
    paths: PlakPaths,
    connection: Arc<Mutex<Option<crate::store::ProductStore>>>,
    #[cfg(test)]
    opens: Arc<std::sync::atomic::AtomicUsize>,
}

impl SqliteSessionStore {
    /// The store behind `paths`' product database.
    #[must_use]
    pub fn new(paths: PlakPaths) -> Self {
        Self {
            paths,
            connection: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            opens: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn opened_connections(&self) -> usize {
        self.opens.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn with_store<T, E: From<String>>(
        &self,
        operation: impl FnOnce(&mut crate::store::ProductStore) -> Result<T, E>,
    ) -> Result<T, E> {
        // A panic rolls SQLite transactions back before the mutex unlocks.
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Retirement and external replacement must never leave this service
        // writing the detached database through its old file descriptor.
        if connection
            .as_ref()
            .is_some_and(|store| !store.matches_path(&self.paths.product_database()))
        {
            *connection = None;
        }
        if connection.is_none() {
            *connection = Some(
                crate::store::ProductStore::open(&self.paths)
                    .map_err(|error| E::from(error.to_string()))?,
            );
            #[cfg(test)]
            self.opens
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let store = connection
            .as_mut()
            .ok_or_else(|| E::from("product store is unavailable".to_owned()))?;
        operation(store)
    }
}

impl SessionRecordStore for SqliteSessionStore {
    fn emit_projected_turn_usages(
        &self,
        terminals: &[plakous_hosted::sessions::projector::ProjectedTurnUsage],
    ) {
        crate::telemetry_store::emit_projected_turn_usages(&self.paths, terminals);
    }
    fn write_host_controls(
        &self,
        id: &str,
        suppress_wake: bool,
        pause: Option<plakous_hosted::sessions::projector::HostPauseCause>,
    ) -> Result<(), String> {
        self.with_store(|store| store.write_host_controls(id, suppress_wake, pause))
    }
    fn load(&self, id: &str) -> Result<SessionRecord, String> {
        crate::store::sessions::canonical_product_session_id(id)?;
        self.with_store(|store| store.load(id))
    }

    fn load_queue_head(
        &self,
        id: &str,
    ) -> Result<plakous_hosted::session_store::SessionQueueHead, String> {
        crate::store::sessions::canonical_product_session_id(id)?;
        self.with_store(|store| store.load_queue_head(id))
    }

    fn save(&self, session: &mut SessionRecord) -> Result<(), String> {
        self.with_store(|store| store.commit(session))
    }

    fn save_submission(
        &self,
        session: &mut SessionRecord,
        submission: &plakous_hosted::sessions::projector::Submission,
    ) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        self.with_store(|store| store.commit_submission(session, submission))
    }

    fn discard_submission(
        &self,
        session: &mut SessionRecord,
        command_id: &str,
        pause_cause: Option<plakous_hosted::sessions::projector::HostPauseCause>,
    ) -> Result<(), String> {
        self.with_store(|store| store.discard_submission(session, command_id, pause_cause))
    }

    fn submit_workflow_request(
        &self,
        session: &mut SessionRecord,
        request: &plakous_hosted::sessions::projector::WorkflowRequest,
    ) -> Result<(), String> {
        self.with_store(|store| store.submit_workflow_request(session, request))
    }

    fn discard_unadmitted_workflow(
        &self,
        session: &mut SessionRecord,
        operation_id: &str,
    ) -> Result<(), String> {
        self.with_store(|store| store.discard_unadmitted_workflow(session, operation_id))
    }

    fn withdraw_unadmitted_workflows(
        &self,
        session: &mut SessionRecord,
    ) -> Result<Vec<plakous_hosted::sessions::TranscriptRow>, String> {
        self.with_store(|store| store.withdraw_unadmitted_workflows(session))
    }

    fn list_for_workspace(&self, workspace: &Path) -> Result<Vec<SessionRecord>, String> {
        self.with_store(|store| store.list_for_workspace(workspace))
    }

    fn load_intents(&self, id: &str) -> Result<ProductIntents, String> {
        self.with_store(|store| store.load_intents(id))
    }

    fn load_inherited_submissions(
        &self,
        id: &str,
    ) -> Result<Vec<plakous_hosted::sessions::projector::Submission>, String> {
        self.with_store(|store| store.load_inherited_submissions(id))
    }

    fn load_views(&self, id: &str) -> Result<Option<SessionViews>, String> {
        self.with_store(|store| store.load_views(id))
    }

    fn commit_projection_views(
        &self,
        id: &str,
        writes: &ViewWrites,
        views: &plakous_hosted::sessions::projector::SessionViewState,
    ) -> Result<ProjectionCommit, String> {
        self.with_store(|store| store.commit_projection_views(id, writes, views))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod contract {
    //! One suite, run over the memory double and the product SQLite store.

    use std::path::{Path, PathBuf};

    use super::{SessionRecordStore, SqliteSessionStore};
    use crate::home::paths::PlakPaths;
    use crate::store::sessions::{ProductCompactReason, SessionWorkflowKind};
    use crate::store::sessions::{SessionRecord, TranscriptRow};
    use plakous_hosted::session_store::MemoryStore;
    use plakous_hosted::sessions::NoticeScope;
    use plakous_hosted::sessions::projector::{
        Delivery, SessionViews, Submission, SubmissionKind, ViewWrites,
    };

    fn record(workspace: &Path) -> SessionRecord {
        let mut session = SessionRecord::new(workspace.to_path_buf());
        session.account = Some("codex".into());
        session.model = Some("offline-loopback-v1".into());
        session.effort = Some("medium".into());
        session
    }

    fn suite(store: &dyn SessionRecordStore, workspace: &Path) {
        // First commit publishes at revision 1; a stale token is refused.
        let mut session = record(workspace);
        assert_eq!(session.store_revision, 0);
        store.save(&mut session).unwrap();
        assert_eq!(session.store_revision, 1);
        let narrow = store.load_queue_head(&session.id).unwrap();
        assert_eq!(narrow.pending_queue, session.pending_queue);
        assert_eq!(narrow.recorded_locator, session.plakous_locator);
        let mut stale = session.clone();
        // A runtime row is shown, never persisted by a record commit; an
        // operator notice is persisted as an intent (spec 02 §2.6).
        session.append_transcript(TranscriptRow::user("hello"));
        session.record_notice(TranscriptRow::warning(NoticeScope::Tree, "noted"));
        store.save(&mut session).unwrap();
        assert_eq!(session.store_revision, 2);
        stale.record_notice(TranscriptRow::warning(NoticeScope::Tree, "elsewhere"));
        assert_eq!(
            store.save(&mut stale),
            Err("product session changed concurrently".to_owned())
        );
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.store_revision, 2);
        assert_eq!(
            loaded.transcript,
            vec![TranscriptRow::warning(NoticeScope::Tree, "noted")]
        );
        assert_eq!(
            store.load("00000000-0000-4000-8000-000000000000"),
            Err("session not found".to_owned())
        );

        // A duplicate identity cannot be inserted as new.
        let mut duplicate = loaded.clone();
        duplicate.store_revision = 0;
        assert_eq!(
            store.save(&mut duplicate),
            Err("product session identity already exists or is invalid".to_owned())
        );

        let item = crate::store::sessions::QueueItem::new("do it");
        session.pending_queue.push(item.clone());
        let submission = submission(&item, "cmd-1");
        let revision = session.store_revision;
        store.save_submission(&mut session, &submission).unwrap();
        assert_eq!(session.store_revision, revision + 1);
        store.save_submission(&mut session, &submission).unwrap();
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            vec![submission.clone()]
        );
        let conflict = Submission {
            canonical_command: "changed".into(),
            ..submission
        };
        assert!(store.save_submission(&mut session, &conflict).is_err());
    }

    fn each_store(test: impl Fn(&dyn SessionRecordStore, &Path)) {
        test(&MemoryStore::new(), Path::new("/tmp/plak-memory-contract"));
        let (_home, paths) = PlakPaths::isolated_test_home();
        test(&SqliteSessionStore::new(paths.clone()), paths.home());
    }

    fn locator() -> plakous::sessions::SessionLocator {
        "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003"
            .parse().unwrap()
    }

    #[test]
    fn partial_ancestor_projection_preserves_the_successor_binding() {
        use plakous::sessions::feed::{FeedLocator, FeedSeq};
        use plakous_hosted::sessions::projector::LineageStep;
        each_store(|store, workspace| {
            let mut session = record(workspace);
            let target = locator();
            session.bind_plakous_locator(target).unwrap();
            store.save(&mut session).unwrap();
            let ancestor = "00000000-0000-4000-8000-000000000004".parse().unwrap();
            let empty = SessionViews::empty();
            let mut partial = SessionViews::empty_with_lineage(vec![
                LineageStep {
                    branch_id: ancestor,
                    from_seq: 0,
                },
                LineageStep {
                    branch_id: target.branch_id(),
                    from_seq: 300,
                },
            ]);
            partial.cursor = FeedSeq::new(256);
            partial.state.runtime_session_id = Some(target.session_id());
            partial.state.head = Some(FeedLocator {
                session_id: target.session_id(),
                branch_id: ancestor,
                checkpoint_id: target.checkpoint_id(),
            });
            store
                .commit_projection_views(
                    &session.id,
                    &ViewWrites::between(&empty, &partial),
                    &partial.state,
                )
                .unwrap();
            assert_eq!(
                store.load(&session.id).unwrap().plakous_locator,
                Some(target)
            );
            let listed = store.list_for_workspace(workspace).unwrap();
            assert_eq!(
                listed
                    .iter()
                    .find(|row| row.id == session.id)
                    .unwrap()
                    .plakous_locator,
                Some(target)
            );
        });
    }

    #[test]
    fn projected_rebind_successors_are_atomic_idempotent_and_keep_the_source_selection() {
        use plakous::sessions::feed::{FeedBranchTransition, FeedLocator, FeedSeq};
        use plakous_hosted::sessions::projector::{
            ProjectionCommit, SessionViews, Successor, ViewWrites, WorkflowRequest,
        };
        each_store(|store, workspace| {
            let mut source = record(workspace);
            source.bind_plakous_locator(locator()).unwrap();
            let note = TranscriptRow::warning(NoticeScope::Tree, "source notice");
            source.record_notice(note.clone());
            store.save(&mut source).unwrap();
            let request = WorkflowRequest {
                operation_id: "plak:rebind:projected-contract".into(),
                source_locator: locator(),
                kind: SessionWorkflowKind::Rebind(Box::new(
                    plakous_hosted::sessions::RebindSelection {
                        provider: "codex".into(),
                        account: "codex".into(),
                        model: "offline-loopback-v2".into(),
                        effort: Some("high".into()),
                        composition: source.composition,
                        target_selection: source.target_selection.clone(),
                        protocol: "responses".into(),
                        route: "offline".into(),
                        requested_model: "offline-loopback-v2".into(),
                        selected_configuration: "selected".into(),
                        effective_configuration: "effective".into(),
                        account_binding: "binding".into(),
                    },
                )),
            };
            store
                .submit_workflow_request(&mut source, &request)
                .unwrap();
            let compact_request = WorkflowRequest {
                operation_id: "plak:compact:projected-contract".into(),
                source_locator: locator(),
                kind: SessionWorkflowKind::Compact(ProductCompactReason::Manual),
            };
            store
                .submit_workflow_request(&mut source, &compact_request)
                .unwrap();
            let source_revision = source.store_revision;
            let mut source_views = SessionViews::empty();
            source_views.cursor = FeedSeq::new(1);
            source_views.state.runtime_session_id = Some(locator().session_id());
            source_views.state.head = Some(FeedLocator {
                session_id: locator().session_id(),
                branch_id: locator().branch_id(),
                checkpoint_id: locator().checkpoint_id(),
            });
            let mut target_views = source_views.clone();
            target_views.state.head.as_mut().unwrap().branch_id =
                "00000000-0000-4000-8000-000000000004".parse().unwrap();
            target_views.state.counters.root_turns = 7;
            let successor = Successor {
                operation_id: Some(request.operation_id.clone()),
                transition: FeedBranchTransition::Rebind,
                created_at_unix: 42,
                target: target_views.state.head.unwrap(),
                published_seq: source_views.cursor,
                seed: None,
            };
            let mut writes = ViewWrites::between(&SessionViews::empty(), &source_views);
            let mut invalid = successor.clone();
            invalid.operation_id = Some(compact_request.operation_id);
            writes.successors = vec![successor.clone(), invalid];
            assert!(
                store
                    .commit_projection_views(&source.id, &writes, &source_views.state)
                    .is_err()
            );
            assert!(store.load_views(&source.id).unwrap().is_none());
            assert!(store.load(&request.successor_product_id()).is_err());
            assert_eq!(
                store.load(&source.id).unwrap().store_revision,
                source_revision
            );
            writes.successors = vec![successor.clone()];
            assert_eq!(
                store
                    .commit_projection_views(&source.id, &writes, &source_views.state)
                    .unwrap(),
                ProjectionCommit::Applied
            );
            let target = store.load(&request.successor_product_id()).unwrap();
            assert_eq!(target.created_at_unix, 42);
            assert_eq!(target.model.as_deref(), Some("offline-loopback-v2"));
            assert_eq!(target.effort.as_deref(), Some("high"));
            assert_eq!(target.root_turn_count, 0);
            assert_eq!(target.transcript, vec![note]);
            assert!(store.load_views(&target.id).unwrap().is_none());

            assert_eq!(store.load(&source.id).unwrap().model, source.model);
            assert_eq!(
                store.load(&source.id).unwrap().store_revision,
                source_revision
            );
            assert_eq!(
                store
                    .commit_projection_views(&source.id, &writes, &source_views.state)
                    .unwrap(),
                ProjectionCommit::CursorMoved
            );
            target_views.cursor = FeedSeq::new(2);
            store
                .commit_projection_views(
                    &target.id,
                    &ViewWrites::between(&SessionViews::empty(), &target_views),
                    &target_views.state,
                )
                .unwrap();
            let before = source_views.clone();
            source_views.cursor = FeedSeq::new(2);
            let mut writes = ViewWrites::between(&before, &source_views);
            writes.successors.push(successor);
            store
                .commit_projection_views(&source.id, &writes, &source_views.state)
                .unwrap();
            assert_eq!(store.load_views(&target.id).unwrap().unwrap(), target_views);
            assert_eq!(store.load_intents(&target.id).unwrap().notices.len(), 1);
            assert_eq!(
                store.load(&target.id).unwrap().store_revision,
                target.store_revision
            );
        });
    }

    #[test]
    fn workflow_requests_are_immutable_cas_intents_not_journal_states() {
        use plakous_hosted::sessions::projector::WorkflowRequest;
        each_store(|store, workspace| {
            let mut session = record(workspace);
            session.bind_plakous_locator(locator()).unwrap();
            store.save(&mut session).unwrap();
            let mut stale = session.clone();
            let request = WorkflowRequest {
                operation_id: "plak:compact:intent".into(),
                source_locator: locator(),
                kind: SessionWorkflowKind::Compact(ProductCompactReason::Manual),
            };
            store
                .submit_workflow_request(&mut session, &request)
                .unwrap();

            assert!(store.submit_workflow_request(&mut stale, &request).is_err());
            let revision = session.store_revision;
            let mut contradictory = request.clone();
            contradictory.kind = SessionWorkflowKind::Fork;
            assert!(
                store
                    .submit_workflow_request(&mut session, &contradictory)
                    .is_err()
            );
            assert_eq!(session.store_revision, revision);
            assert_eq!(
                store.load_intents(&session.id).unwrap().workflow_requests,
                vec![request]
            );
        });
    }

    #[test]
    fn live_workflow_discard_is_cas_guarded_and_preserves_recorded_requests_and_bindings() {
        use plakous_hosted::sessions::projector::{
            SessionViews, ViewWrites, WorkflowCompletion, WorkflowRequest, WorkflowSettlement,
        };
        each_store(|store, workspace| {
            let mut session = record(workspace);
            session.bind_plakous_locator(locator()).unwrap();
            store.save(&mut session).unwrap();
            for operation_id in [
                "plak:compact:unadmitted",
                "plak:compact:admitted",
                "plak:compact:failed",
            ] {
                store
                    .submit_workflow_request(
                        &mut session,
                        &WorkflowRequest {
                            operation_id: operation_id.into(),
                            source_locator: locator(),
                            kind: SessionWorkflowKind::Compact(ProductCompactReason::Scheduled),
                        },
                    )
                    .unwrap();
            }
            assert!(
                store
                    .discard_unadmitted_workflow(&mut session, "plak:compact:unadmitted")
                    .is_err()
            );
            let mut views = SessionViews::empty();
            views.state.runtime_session_id = Some(locator().session_id());
            views.state.workflows.insert(
                "plak:compact:admitted".into(),
                WorkflowCompletion {
                    admitted_at: Some(0),
                    settled: None,
                },
            );
            views.state.workflows.insert(
                "plak:compact:failed".into(),
                WorkflowCompletion {
                    admitted_at: Some(0),
                    settled: Some(WorkflowSettlement::Failed),
                },
            );
            store
                .commit_projection_views(
                    &session.id,
                    &ViewWrites {
                        successors: Vec::new(),
                        expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                        cursor: plakous::sessions::feed::FeedSeq::ZERO,
                        upserts: Vec::new(),
                        deletes: Vec::new(),
                        calls: Vec::new(),
                        open_turns: Vec::new(),
                        processes: Vec::new(),
                    },
                    &views.state,
                )
                .unwrap();
            let mut stale = session.clone();
            store.save(&mut session).unwrap();
            assert!(
                store
                    .discard_unadmitted_workflow(&mut stale, "plak:compact:unadmitted")
                    .is_err()
            );
            for operation_id in ["plak:compact:admitted", "plak:compact:failed"] {
                assert!(
                    store
                        .discard_unadmitted_workflow(&mut session, operation_id)
                        .is_err()
                );
            }
            assert_eq!(
                store
                    .load_intents(&session.id)
                    .unwrap()
                    .workflow_requests
                    .len(),
                3
            );
            store
                .discard_unadmitted_workflow(&mut session, "plak:compact:unadmitted")
                .unwrap();
            let intents = store.load_intents(&session.id).unwrap();
            assert_eq!(intents.workflow_requests.len(), 2);
            assert_eq!(
                intents.notices,
                [] as [plakous_hosted::sessions::projector::OperatorNotice; 0]
            );
            let revision = session.store_revision;
            store
                .discard_unadmitted_workflow(&mut session, "plak:compact:unadmitted")
                .unwrap();
            assert_eq!(session.store_revision, revision);
            assert!(
                store
                    .submit_workflow_request(
                        &mut session,
                        &WorkflowRequest {
                            operation_id: "plak:compact:unadmitted".into(),
                            source_locator: locator(),
                            kind: SessionWorkflowKind::Compact(ProductCompactReason::Scheduled),
                        }
                    )
                    .is_err(),
                "a discarded identity cannot be replayed even with identical bytes"
            );
            assert!(
                store
                    .submit_workflow_request(
                        &mut session,
                        &WorkflowRequest {
                            operation_id: "plak:compact:unadmitted".into(),
                            source_locator: locator(),
                            kind: SessionWorkflowKind::Clear,
                        }
                    )
                    .is_err()
            );
        });
    }

    #[test]
    fn workflow_withdrawal_is_atomic_once_and_preserves_admitted_requests() {
        use plakous_hosted::sessions::projector::{
            SessionViews, ViewWrites, WorkflowCompletion, WorkflowRequest,
        };
        each_store(|store, workspace| {
            let mut session = record(workspace);
            session.bind_plakous_locator(locator()).unwrap();
            store.save(&mut session).unwrap();
            for (id, kind) in [
                ("plak:fork:withdraw", SessionWorkflowKind::Fork),
                ("plak:clear:withdraw", SessionWorkflowKind::Clear),
                (
                    "plak:compact:admitted",
                    SessionWorkflowKind::Compact(ProductCompactReason::Manual),
                ),
            ] {
                store
                    .submit_workflow_request(
                        &mut session,
                        &WorkflowRequest {
                            operation_id: id.into(),
                            source_locator: locator(),
                            kind,
                        },
                    )
                    .unwrap();
            }
            assert!(store.withdraw_unadmitted_workflows(&mut session).is_err());
            let mut views = SessionViews::empty();
            views.state.runtime_session_id = Some(locator().session_id());
            views.state.workflows.insert(
                "plak:compact:admitted".into(),
                WorkflowCompletion {
                    admitted_at: Some(0),
                    settled: None,
                },
            );
            store
                .commit_projection_views(
                    &session.id,
                    &ViewWrites {
                        expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                        cursor: plakous::sessions::feed::FeedSeq::ZERO,
                        upserts: Vec::new(),
                        deletes: Vec::new(),
                        calls: Vec::new(),
                        open_turns: Vec::new(),
                        processes: Vec::new(),
                        successors: Vec::new(),
                    },
                    &views.state,
                )
                .unwrap();
            let mut stale = session.clone();
            session.touch();
            store.save(&mut session).unwrap();
            assert!(store.withdraw_unadmitted_workflows(&mut stale).is_err());
            assert_eq!(
                store
                    .load_intents(&session.id)
                    .unwrap()
                    .workflow_requests
                    .len(),
                3
            );
            let rows = store.withdraw_unadmitted_workflows(&mut session).unwrap();
            assert_eq!(
                rows,
                vec![
                    TranscriptRow::info(
                        NoticeScope::Recovery,
                        "fork did not start; run /fork again"
                    ),
                    TranscriptRow::info(
                        NoticeScope::Recovery,
                        "clear did not start; run /clear again"
                    ),
                ]
            );
            assert_eq!(
                store.withdraw_unadmitted_workflows(&mut session).unwrap(),
                [] as [TranscriptRow; 0]
            );
            let intents = store.load_intents(&session.id).unwrap();
            assert_eq!(intents.workflow_requests.len(), 1);
            assert_eq!(intents.notices.len(), 2);
            assert_eq!(intents.notices[0].ordinal, 0);
            assert_eq!(intents.notices[1].ordinal, 1);
        });
    }

    #[test]
    fn inherited_submissions_read_only_same_runtime_ancestry_and_stop_at_import() {
        each_store(|store, workspace| {
            let mut source = queued(workspace);
            source.bind_plakous_locator(locator()).unwrap();
            let submitted = submission(&source.pending_queue[0], "ancestor-root");
            store.save_submission(&mut source, &submitted).unwrap();
            let mut child = record(workspace);
            child.bind_plakous_locator(locator()).unwrap();
            child.source_product_session_id = Some(source.id.clone());
            child.source_workflow_operation_id = Some("plak:fork:ancestry".into());
            store.save(&mut child).unwrap();
            let mut grandchild = record(workspace);
            grandchild.bind_plakous_locator(locator()).unwrap();
            grandchild.source_product_session_id = Some(child.id.clone());
            grandchild.source_workflow_operation_id = Some("plak:fork:ancestry-next".into());
            store.save(&mut grandchild).unwrap();
            assert_eq!(
                store.load_inherited_submissions(&grandchild.id).unwrap(),
                vec![submitted]
            );

            let mut carried = record(workspace);
            carried.bind_plakous_locator(locator()).unwrap();
            carried.link_carried_forward(&uuid::Uuid::new_v4().to_string(), "previous-build");
            store.save(&mut carried).unwrap();
            assert_eq!(
                store.load_inherited_submissions(&carried.id).unwrap(),
                [] as [Submission; 0]
            );

            let mut cleared = record(workspace);
            cleared
                .bind_plakous_locator(plakous::sessions::SessionLocator::new(
                    uuid::Uuid::new_v4().try_into().unwrap(),
                    locator().branch_id(),
                    locator().checkpoint_id(),
                ))
                .unwrap();
            cleared.source_product_session_id = Some(source.id.clone());
            cleared.source_workflow_operation_id = Some("plak:clear:ancestry".into());
            store.save(&mut cleared).unwrap();
            assert_eq!(
                store.load_inherited_submissions(&cleared.id).unwrap(),
                [] as [Submission; 0]
            );
        });
    }

    #[test]
    fn deleted_clear_reservation_does_not_block_open_or_one_shot() {
        use crate::test_support::{ActualHostAction, ActualHostInjection, run_actual_host};
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
            SessionOperationId,
        };
        use plakous_hosted::sessions::projector::WorkflowRequest;
        use plakous_offline_crash_harness::{BoundaryController, runtime_binding};
        use std::sync::Arc;

        for one_shot in [false, true] {
            let (_home, paths) = PlakPaths::isolated_test_home();
            paths.ensure_layout().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let controller = BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                workspace.path().join("clear-deletion-evidence.jsonl"),
            )
            .unwrap();
            let mut source = record(workspace.path());
            let runtime = runtime_binding(workspace.path(), controller.clone()).unwrap();
            let target = runtime.target_binding();
            let sessions =
                AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap())
                    .unwrap();
            let executor = plakous::application::ApplicationExecutor::new().unwrap();
            let locator = executor
                .block_on(async {
                    let CreateSessionOutcome::Created { session, .. } = sessions
                        .create(CreateSessionRequest::new(
                            SessionOperationId::new(&source.create_operation_id).unwrap(),
                            runtime,
                        ))
                        .await
                        .unwrap()
                    else {
                        panic!("created")
                    };
                    session.release().await.unwrap()
                })
                .unwrap();
            source.bind_plakous_locator(locator).unwrap();
            let store = SqliteSessionStore::new(paths.clone());
            store.save(&mut source).unwrap();
            let request = WorkflowRequest {
                operation_id: "plak:clear:deleted-reservation".into(),
                source_locator: locator,
                kind: SessionWorkflowKind::Clear,
            };
            store
                .submit_workflow_request(&mut source, &request)
                .unwrap();
            let reserved = store.load(&request.successor_product_id()).unwrap();
            store
                .with_store(|store| store.delete_session(&reserved.id, reserved.store_revision))
                .unwrap();
            for pass in 0..2 {
                let workspace = workspace.path().to_owned();
                let controller = controller.clone();
                let injection = ActualHostInjection::new(
                    Arc::new(move |_| {
                        runtime_binding(&workspace, controller.clone())
                            .map_err(|error| error.to_string())
                    }),
                    target.clone(),
                    Arc::new(|_, _| {}),
                );
                let actions = if one_shot && pass == 0 {
                    vec![ActualHostAction::PromptOnce("after deletion".into())]
                } else {
                    vec![ActualHostAction::Quit]
                };
                run_actual_host(paths.home().to_owned(), &source.id, injection, actions).unwrap();
                let intents = store.load_intents(&source.id).unwrap();
                assert_eq!(intents.workflow_requests, [] as [WorkflowRequest; 0]);
                assert_eq!(
                    intents
                        .notices
                        .iter()
                        .filter(|notice| notice.notice_id
                            == format!("withdrawn-workflow:{}", request.operation_id))
                        .count(),
                    1
                );
                assert!(store.load(&reserved.id).is_err());
            }
        }
    }

    #[test]
    fn clear_withdrawal_refuses_a_successor_with_views_without_losing_its_intent() {
        use plakous_hosted::sessions::projector::{SessionViews, ViewWrites, WorkflowRequest};
        each_store(|store, workspace| {
            let mut session = record(workspace);
            session.bind_plakous_locator(locator()).unwrap();
            store.save(&mut session).unwrap();
            let request = WorkflowRequest {
                operation_id: "plak:clear:observed-reservation".into(),
                source_locator: locator(),
                kind: SessionWorkflowKind::Clear,
            };
            store
                .submit_workflow_request(&mut session, &request)
                .unwrap();
            let empty = SessionViews::empty();
            let writes = ViewWrites::between(&empty, &empty);
            store
                .commit_projection_views(&session.id, &writes, &empty.state)
                .unwrap();
            store
                .commit_projection_views(&request.successor_product_id(), &writes, &empty.state)
                .unwrap();
            let revision = session.store_revision;
            assert!(store.withdraw_unadmitted_workflows(&mut session).is_err());
            assert_eq!(session.store_revision, revision);
            assert_eq!(store.load(&session.id).unwrap().store_revision, revision);
            assert_eq!(
                store.load_intents(&session.id).unwrap().workflow_requests,
                vec![request.clone()]
            );
            assert_eq!(
                store.load_intents(&session.id).unwrap().notices,
                [] as [plakous_hosted::sessions::projector::OperatorNotice; 0]
            );
            assert!(store.load(&request.successor_product_id()).is_ok());
        });
    }

    fn queued(workspace: &Path) -> SessionRecord {
        let mut session = record(workspace);
        session
            .pending_queue
            .push(crate::store::sessions::QueueItem::new("pending"));
        session
    }

    fn submission(item: &crate::store::sessions::QueueItem, command: &str) -> Submission {
        Submission {
            command_id: command.into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        }
    }

    fn project_delivery(
        store: &dyn SessionRecordStore,
        id: &str,
        command: &str,
        delivery: Delivery,
    ) {
        let before = store
            .load_views(id)
            .unwrap()
            .unwrap_or_else(SessionViews::empty);
        let mut views = before.clone();
        views.state.runtime_session_id = Some(locator().session_id());
        views.state.deliveries.insert(command.into(), delivery);
        store
            .commit_projection_views(id, &ViewWrites::between(&before, &views), &views.state)
            .unwrap();
    }

    fn activity(call_id: &str, state: &str) -> crate::store::sessions::ToolItem {
        crate::store::sessions::ToolItem {
            call_id: call_id.into(),
            name: "read_file".into(),
            exposed_name: None,
            kind: plakous_host_protocol::views::presentation::ActivityKind::Tool,
            state: state.into(),
            subject: None,
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        }
    }

    #[test]
    fn queue_load_joins_submissions_and_views_without_writing_intents() {
        use plakous::sessions::feed::FeedSeq;

        each_store(|store, workspace| {
            let mut session = queued(workspace);
            session.queue_projection_debt = true;
            let item = session.pending_queue[0].clone();
            let submission = Submission {
                command_id: "queue:joined".into(),
                kind: SubmissionKind::RootStart,
                queue_item_id: item.id.clone(),
                queue_item_revision: item.revision,
                target_turn_id: None,
                canonical_command: item.text.clone(),
                submitted_at_ms: item.submitted_at_ms,
            };
            store.save_submission(&mut session, &submission).unwrap();
            let revision = session.store_revision;
            assert!(session.queue_commands.is_locked(&item.id));
            assert!(!store.load(&session.id).unwrap().queue_projection_debt);
            let mut views = SessionViews::empty();
            views.state.runtime_session_id = Some(locator().session_id());
            let writes = ViewWrites {
                successors: Vec::new(),
                expected_cursor: FeedSeq::ZERO,
                cursor: FeedSeq::ZERO,
                upserts: Vec::new(),
                deletes: Vec::new(),
                calls: Vec::new(),
                open_turns: Vec::new(),
                processes: Vec::new(),
            };
            views.state.deliveries.insert(
                submission.command_id.clone(),
                Delivery::Rejected {
                    error: "busy".into(),
                },
            );
            store
                .commit_projection_views(&session.id, &writes, &views.state)
                .unwrap();
            let loaded = store.load(&session.id).unwrap();
            assert_eq!(loaded.store_revision, revision);
            assert_eq!(loaded.pending_queue, vec![item.clone()]);
            assert!(loaded.queue_commands.is_rejected(&item));
            assert!(!loaded.queue_commands.is_locked(&item.id));

            views
                .state
                .deliveries
                .insert(submission.command_id.clone(), Delivery::Accepted);
            store
                .commit_projection_views(&session.id, &writes, &views.state)
                .unwrap();
            let loaded = store.load(&session.id).unwrap();
            assert_eq!(loaded.store_revision, revision);
            assert_eq!(
                loaded.pending_queue,
                [] as [plakous_hosted::sessions::QueueItem; 0]
            );
            let narrow = store.load_queue_head(&session.id).unwrap();
            assert_eq!(narrow.pending_queue, vec![item]);
            assert_eq!(narrow.recorded_locator, session.plakous_locator);
            assert_eq!(
                store.load_intents(&session.id).unwrap().submissions,
                vec![submission]
            );
            assert_eq!(
                store.load(&session.id).unwrap().queue_commands,
                loaded.queue_commands
            );
        });
    }

    #[test]
    fn submission_and_queue_commit_under_one_revision_cas_and_identity_binding() {
        each_store(|store, workspace| {
            let mut session = record(workspace);
            store.save(&mut session).unwrap();
            let stale = session.clone();
            let item = crate::store::sessions::QueueItem::new("root with child mail");
            session.pending_queue.push(item.clone());
            let submission = Submission {
                command_id: "submission:root".into(),
                kind: SubmissionKind::RootStart,
                queue_item_id: item.id.clone(),
                queue_item_revision: item.revision,
                target_turn_id: None,
                canonical_command: "root with child mail\nexact batch".into(),
                submitted_at_ms: item.submitted_at_ms,
            };
            store.save_submission(&mut session, &submission).unwrap();
            assert_eq!(
                store.load_intents(&session.id).unwrap().submissions,
                vec![submission.clone()]
            );
            assert_eq!(store.load(&session.id).unwrap().pending_queue, vec![item]);

            let mut conflicting = session.clone();
            conflicting.pending_queue[0].text = "different".into();
            let conflicting_intent = Submission {
                canonical_command: "different".into(),
                ..submission.clone()
            };
            let revision = conflicting.store_revision;
            assert!(
                store
                    .save_submission(&mut conflicting, &conflicting_intent)
                    .is_err()
            );
            assert_eq!(conflicting.store_revision, revision);
            assert_eq!(
                store.load(&session.id).unwrap().pending_queue,
                session.pending_queue
            );

            let mut stale = stale;
            let uncommitted = Submission {
                command_id: "submission:stale".into(),
                ..submission.clone()
            };
            assert!(store.save_submission(&mut stale, &uncommitted).is_err());
            assert_eq!(
                store.load_intents(&session.id).unwrap().submissions,
                vec![submission.clone()]
            );
            store.save_submission(&mut session, &submission).unwrap();
            assert_eq!(
                store.load_intents(&session.id).unwrap().submissions,
                vec![submission.clone()]
            );
            let pause_cause =
                plakous_hosted::sessions::projector::HostPauseCause::UnrecordedRefusal {
                    reason: "session store is full".into(),
                };
            assert!(
                store
                    .discard_submission(
                        &mut stale,
                        &submission.command_id,
                        Some(pause_cause.clone())
                    )
                    .is_err()
            );
            assert_eq!(
                store.load_intents(&session.id).unwrap().pause_causes,
                [] as [plakous_hosted::sessions::projector::AnchoredIntent<
                    plakous_hosted::sessions::projector::HostPauseCause,
                >; 0]
            );
            assert_eq!(
                store.load_intents(&session.id).unwrap().submissions,
                vec![submission.clone()]
            );
            session.pending_queue.clear();
            store
                .discard_submission(
                    &mut session,
                    &submission.command_id,
                    Some(pause_cause.clone()),
                )
                .unwrap();
            assert_eq!(
                store
                    .load_intents(&session.id)
                    .unwrap()
                    .pause_causes
                    .iter()
                    .map(|intent| &intent.value)
                    .collect::<Vec<_>>(),
                vec![&pause_cause]
            );
            assert_eq!(
                store.load_intents(&session.id).unwrap().submissions,
                [] as [Submission; 0]
            );
            assert_eq!(
                store.load(&session.id).unwrap().pending_queue,
                [] as [plakous_hosted::sessions::QueueItem; 0]
            );
            let revision = session.store_revision;
            for conflicting in [
                Submission {
                    canonical_command: "changed after withdrawal".into(),
                    ..submission.clone()
                },
                Submission {
                    kind: SubmissionKind::SendNowSteer,
                    target_turn_id: Some("other-turn".into()),
                    ..submission.clone()
                },
                Submission {
                    submitted_at_ms: submission.submitted_at_ms + 1,
                    ..submission.clone()
                },
            ] {
                assert!(store.save_submission(&mut session, &conflicting).is_err());
                assert_eq!(session.store_revision, revision);
                assert_eq!(
                    store.load_intents(&session.id).unwrap().submissions,
                    [] as [Submission; 0]
                );
            }
        });
    }

    #[test]
    fn notice_intent_commits_are_cas_bound_and_a_refused_notice_rides_the_next_commit() {
        each_store(|store, workspace| {
            let mut session = record(workspace);
            store.save(&mut session).unwrap();
            let stale = session.clone();

            session.append_transcript(TranscriptRow::user("go"));
            session.upsert_tool_activity(activity("a", "running"));
            session.record_notice(TranscriptRow::warning(NoticeScope::Tree, "first"));
            store.save(&mut session).unwrap();
            assert_eq!(session.transcript.pending_notices(), []);
            let loaded = store.load(&session.id).unwrap();
            assert_eq!(loaded.store_revision, session.store_revision);
            assert_eq!(
                loaded.transcript,
                vec![TranscriptRow::warning(NoticeScope::Tree, "first")],
                "only the notice is persisted"
            );

            // A stale record is refused and keeps its notice pending.
            let mut stale = stale;
            stale.record_notice(TranscriptRow::warning(NoticeScope::Tree, "elsewhere"));
            assert_eq!(
                store.save(&mut stale),
                Err("product session changed concurrently".to_owned())
            );
            assert_eq!(stale.transcript.pending_notices().len(), 1);

            // A notice whose projection commit was refused rides the next
            // authority commit rather than being lost.
            session.record_notice(TranscriptRow::warning(NoticeScope::Tree, "second"));
            session.title = Some("answered".into());
            store.save(&mut session).unwrap();
            let loaded = store.load(&session.id).unwrap();
            assert_eq!(
                loaded.transcript,
                vec![
                    TranscriptRow::warning(NoticeScope::Tree, "first"),
                    TranscriptRow::warning(NoticeScope::Tree, "second"),
                ]
            );
            assert_eq!(loaded.title.as_deref(), Some("answered"));
        });
    }

    #[test]
    fn host_controls_are_intents_not_session_flags() {
        use plakous_hosted::sessions::projector::{
            Anchored, HostPauseCause, SessionViews, ViewWrites,
        };

        each_store(|store, workspace| {
            let mut session = record(workspace);
            store.save(&mut session).unwrap();
            let reason = HostPauseCause::WakePreflightFailed {
                reason: "does not fit".to_owned(),
            };
            store
                .write_host_controls(&session.id, true, Some(reason.clone()))
                .unwrap();
            let intents = store.load_intents(&session.id).unwrap();
            let views = SessionViews::empty();
            assert!(views.wake_suppressed(&intents));
            assert_eq!(
                views.pause_cause(&intents),
                Some(plakous_hosted::sessions::projector::PauseCause::Host(
                    reason
                ))
            );
            let loaded = store.load(&session.id).unwrap();
            assert!(loaded.child_wake_suppressed);
            assert_eq!(
                loaded.queue_drain_state,
                crate::store::sessions::QueueDrainState::RestoredPaused
            );

            let mut resumed = views.clone();
            resumed.cursor = plakous::sessions::feed::FeedSeq::new(1);
            resumed.state.wake = Some(Anchored {
                anchor: 1,
                value: false,
            });
            resumed.state.pause = Some(Anchored {
                anchor: 1,
                value: None,
            });
            store
                .commit_projection_views(
                    &session.id,
                    &ViewWrites::between(&views, &resumed),
                    &resumed.state,
                )
                .unwrap();
            let loaded = store.load(&session.id).unwrap();
            assert!(!loaded.child_wake_suppressed);
            assert_eq!(
                loaded.queue_drain_state,
                crate::store::sessions::QueueDrainState::LiveAutoDrain
            );
        });
    }

    #[test]
    fn restored_loads_pause_outstanding_work_without_publishing_the_projection() {
        use crate::store::sessions::QueueDrainState;

        each_store(|store, workspace| {
            for initial in [
                QueueDrainState::LiveAutoDrain,
                QueueDrainState::RestoredPaused,
            ] {
                let cases = vec![(record(workspace), false), (queued(workspace), true)];
                for (mut session, outstanding) in cases {
                    session.queue_drain_state = initial;

                    store.save(&mut session).unwrap();
                    let before = store.list_for_workspace(workspace).unwrap();
                    let revision = session.store_revision;
                    let mut expected = session.clone();
                    expected.queue_drain_state = if outstanding {
                        QueueDrainState::RestoredPaused
                    } else {
                        QueueDrainState::LiveAutoDrain
                    };
                    assert_eq!(store.load(&session.id).unwrap(), expected);
                    assert_eq!(store.load(&session.id).unwrap(), expected);
                    assert_eq!(
                        store.list_for_workspace(workspace).unwrap(),
                        before,
                        "loading must not publish the recovery projection"
                    );
                    assert_eq!(session.queue_drain_state, initial);
                    // The caller's pre-load CAS token still commits, exactly once.
                    store.save(&mut session).unwrap();
                    assert_eq!(session.store_revision, revision + 1);
                }
            }
        });
    }

    #[test]
    fn settled_submissions_preserve_exact_replay_after_withdrawal_and_view_rebuild() {
        for terminal in [
            Delivery::Rejected {
                error: "refused".into(),
            },
            Delivery::Accepted,
        ] {
            each_store(|store, workspace| {
                let mut session = queued(workspace);
                let item = session.pending_queue[0].clone();
                let request = submission(&item, "terminal-command");
                store.save_submission(&mut session, &request).unwrap();
                project_delivery(store, &session.id, &request.command_id, terminal.clone());
                let loaded = store.load(&session.id).unwrap();
                assert_eq!(
                    loaded.pending_queue.is_empty(),
                    terminal == Delivery::Accepted
                );
                assert!(!loaded.queue_commands.is_locked(&item.id));
                assert_eq!(
                    store.load_intents(&session.id).unwrap().submissions,
                    vec![request.clone()]
                );
                let revision = session.store_revision;
                let mut invalid = request.clone();
                invalid.canonical_command = "contradictory body".into();
                assert!(store.save_submission(&mut session, &invalid).is_err());
                assert_eq!(session.store_revision, revision);
                assert_eq!(store.load(&session.id).unwrap(), loaded);
                store
                    .discard_submission(&mut session, &request.command_id, None)
                    .unwrap();
                assert_eq!(
                    store.load_intents(&session.id).unwrap().submissions,
                    [] as [Submission; 0]
                );
                // Rebuilding the views cannot erase the immutable content binding.
                let before = store.load_views(&session.id).unwrap().unwrap();
                let empty = SessionViews::empty();
                store
                    .commit_projection_views(
                        &session.id,
                        &ViewWrites::between(&before, &empty),
                        &empty.state,
                    )
                    .unwrap();
                assert!(store.save_submission(&mut session, &invalid).is_err());
                store.save_submission(&mut session, &request).unwrap();
                assert_eq!(
                    store.load_intents(&session.id).unwrap().submissions,
                    vec![request]
                );
            });
        }
    }

    #[test]
    fn submission_rejects_invalid_queue_without_writes_or_identity_reservations() {
        for transfer_claim in [false, true] {
            each_store(|store, workspace| {
                let mut session = queued(workspace);
                store.save(&mut session).unwrap();
                let before = store.load(&session.id).unwrap();
                let listed_before = store.list_for_workspace(workspace).unwrap();
                let item = crate::store::sessions::QueueItem::new("second command");
                session.pending_queue.push(item.clone());
                let valid = session.clone();
                session.pending_queue[0].text.clear();
                let invalid = session.clone();
                let request = submission(&item, "second-command");
                assert!(store.save_submission(&mut session, &request).is_err());
                assert_eq!(session, invalid, "failure cannot mutate the caller");
                assert_eq!(store.load(&session.id).unwrap(), before);
                assert_eq!(store.list_for_workspace(workspace).unwrap(), listed_before);
                assert_eq!(
                    store.load_intents(&session.id).unwrap().submissions,
                    [] as [Submission; 0]
                );
                let mut retry = if transfer_claim {
                    let mut other = record(workspace);
                    other.pending_queue.push(item.clone());
                    other
                } else {
                    valid
                };
                let revision = retry.store_revision;
                store.save_submission(&mut retry, &request).unwrap();
                assert_eq!(retry.store_revision, revision + 1);
                assert_eq!(
                    store.load_intents(&retry.id).unwrap().submissions,
                    vec![request]
                );
                if transfer_claim {
                    assert_eq!(store.load(&session.id).unwrap(), before);
                }
            });
        }
    }

    #[test]
    fn sqlite_recovery_projection_never_updates_the_stored_row() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        let store = SqliteSessionStore::new(paths.clone());
        let mut session = queued(paths.home());
        store.save(&mut session).unwrap();
        let snapshot = || {
            store
                .with_store(|database| {
                    database
                        .connection
                        .query_row(
                            "SELECT revision FROM sessions WHERE id=?1",
                            [&session.id],
                            |row| row.get::<_, i64>(0),
                        )
                        .map_err(|error| error.to_string())
                })
                .unwrap()
        };
        let before = snapshot();
        assert_eq!(before, i64::try_from(session.store_revision).unwrap());
        assert_eq!(
            store.load(&session.id).unwrap().queue_drain_state,
            crate::store::sessions::QueueDrainState::RestoredPaused
        );
        assert_eq!(snapshot(), before);
    }

    #[test]
    fn rejected_submission_save_leaves_no_reserved_identities() {
        each_store(|store, workspace| {
            let mut owner = queued(workspace);
            let owner_request = submission(&owner.pending_queue[0], "claimed-command");
            store.save_submission(&mut owner, &owner_request).unwrap();
            let mut refused = queued(workspace);
            let refused_request = submission(&refused.pending_queue[0], "claimed-command");
            assert!(
                store
                    .save_submission(&mut refused, &refused_request)
                    .is_err()
            );
            assert_eq!(refused.store_revision, 0);
            assert_eq!(store.load(&refused.id).unwrap_err(), "session not found");
            let mut retry = record(workspace);
            retry.create_operation_id = refused.create_operation_id;
            retry.pending_queue = refused.pending_queue;
            let retry_request = submission(&retry.pending_queue[0], "new-command");
            store.save_submission(&mut retry, &retry_request).unwrap();
            assert_eq!(retry.store_revision, 1);
        });
    }

    #[test]
    fn queue_owner_edits_and_deleted_identity_are_durable() {
        each_store(|store, workspace| {
            let mut owner = queued(workspace);
            store.save(&mut owner).unwrap();
            let item = owner.pending_queue[0].clone();
            let mut intruder = record(workspace);
            intruder.pending_queue.push(item.clone());
            assert_eq!(
                store.save(&mut intruder).unwrap_err(),
                "stable product identity contradicts durable state"
            );
            assert_eq!(intruder.store_revision, 0);

            owner.pending_queue[0].text = "edited".into();
            assert_eq!(
                store.save(&mut owner).unwrap_err(),
                "queue identity contradicts durable product state"
            );
            assert_eq!(owner.store_revision, 1);
            assert_eq!(store.load(&owner.id).unwrap().pending_queue[0], item);
            owner.pending_queue[0].revision += 1;
            store.save(&mut owner).unwrap();
            owner.pending_queue[0].revision += 2;
            assert_eq!(
                store.save(&mut owner).unwrap_err(),
                "queue identity contradicts durable product state"
            );
            owner.pending_queue.clear();
            store.save(&mut owner).unwrap();
            owner.pending_queue.push(item);
            assert_eq!(
                store.save(&mut owner).unwrap_err(),
                "stable product identity contradicts durable state"
            );
            assert_eq!(
                store.load(&owner.id).unwrap().pending_queue,
                [] as [plakous_hosted::sessions::QueueItem; 0]
            );
        });
    }

    #[test]
    fn removed_queue_identity_cannot_be_reused_after_submission_withdrawal() {
        each_store(|store, workspace| {
            let mut session = queued(workspace);
            let item = session.pending_queue[0].clone();
            let request = submission(&item, "consume-command");
            store.save_submission(&mut session, &request).unwrap();
            project_delivery(store, &session.id, &request.command_id, Delivery::Accepted);
            assert_eq!(
                store.load(&session.id).unwrap().pending_queue,
                [] as [plakous_hosted::sessions::QueueItem; 0]
            );
            session.pending_queue.clear();
            store
                .discard_submission(&mut session, &request.command_id, None)
                .unwrap();
            session.pending_queue.push(item);
            assert_eq!(
                store.save(&mut session).unwrap_err(),
                "stable product identity contradicts durable state"
            );
            assert_eq!(
                store.load(&session.id).unwrap().pending_queue,
                [] as [plakous_hosted::sessions::QueueItem; 0]
            );
        });
    }

    #[test]
    fn the_memory_store_honours_the_contract() {
        suite(&MemoryStore::new(), &PathBuf::from("/tmp/plak-contract"));
    }

    #[test]
    fn invalid_identity_and_failed_open_do_not_initialize_the_service() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        let store = SqliteSessionStore::new(paths.clone());
        assert_eq!(
            store.load("../invalid").unwrap_err(),
            crate::store::sessions::canonical_product_session_id("../invalid").unwrap_err()
        );
        assert_eq!(store.opened_connections(), 0);
        paths.ensure_layout().unwrap();
        std::fs::create_dir(paths.product_database()).unwrap();
        let mut session = record(paths.home());
        assert!(store.save(&mut session).is_err());
        assert_eq!(store.opened_connections(), 0);
        std::fs::remove_dir(paths.product_database()).unwrap();
        store.save(&mut session).unwrap();
        assert_eq!(store.opened_connections(), 1);
    }

    #[test]
    fn the_sqlite_store_honours_the_contract() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        let workspace = paths.home().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        suite(&SqliteSessionStore::new(paths), &workspace);
    }
}
