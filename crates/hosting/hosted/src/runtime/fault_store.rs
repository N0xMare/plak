//! Fault policies share one complete session-record-store delegate.
use crate::session_store::SessionRecordStore;
use crate::sessions::{self, SessionRecord, TranscriptRow};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreCall {
    WriteHostControls,
    WriteRootLoss,
    Load,
    LoadQueueHead,
    Save,
    SaveSubmission,
    DiscardSubmission,
    SubmitWorkflowRequest,
    DiscardUnadmittedWorkflow,
    WithdrawUnadmittedWorkflows,
    ListForWorkspace,
    LoadIntents,
    LoadInheritedSubmissions,
    LoadViews,
    LoadQueueCommands,
    CommitProjectionViews,
}

pub(crate) trait StoreFaults: Send + Sync {
    fn set_storage_interrupt(&self, _probe: Arc<dyn Fn() -> bool + Send + Sync>) {}
    fn storage_wait_cancelled(&self) -> bool {
        false
    }
    fn storage_unavailable(&self) -> bool {
        false
    }
    fn storage_admission(&self) -> Option<crate::session_store::StorageAdmission> {
        None
    }

    fn before(&self, call: StoreCall, session: Option<&SessionRecord>) -> Result<(), String>;

    /// A transient refusal for the calls whose port error carries one.
    fn refusal(&self, _call: StoreCall) -> Option<crate::error::StoreRefusal> {
        None
    }

    fn before_write(
        &self,
        call: StoreCall,
        session: Option<&SessionRecord>,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        if let Some(refusal) = self.refusal(call) {
            return Err(crate::session_store::RecordStoreError::refused(
                refusal,
                "injected store write refusal",
            ));
        }
        self.before(call, session).map_err(Into::into)
    }
}

pub(crate) struct FaultStore<F> {
    pub(super) inner: Arc<dyn SessionRecordStore>,
    pub(crate) faults: Arc<F>,
}
impl<F> FaultStore<F> {
    pub(crate) fn new(inner: Arc<dyn SessionRecordStore>, faults: F) -> Self {
        Self {
            inner,
            faults: Arc::new(faults),
        }
    }
}

pub(crate) struct RefuseAll(String);
impl StoreFaults for RefuseAll {
    fn before(&self, _call: StoreCall, _session: Option<&SessionRecord>) -> Result<(), String> {
        Err(self.0.clone())
    }
}
impl FaultStore<RefuseAll> {
    pub(crate) fn refusing(reason: impl Into<String>) -> Self {
        Self::new(
            Arc::new(crate::session_store::MemoryStore::new()),
            RefuseAll(reason.into()),
        )
    }
}

impl<F: StoreFaults + 'static> SessionRecordStore for FaultStore<F> {
    fn with_storage_interrupt(
        self: Arc<Self>,
        probe: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Arc<dyn SessionRecordStore> {
        self.faults.set_storage_interrupt(probe.clone());
        Arc::new(Self {
            inner: Arc::clone(&self.inner).with_storage_interrupt(probe),
            faults: Arc::clone(&self.faults),
        })
    }
    fn storage_wait_cancelled(&self) -> bool {
        self.faults.storage_wait_cancelled() || self.inner.storage_wait_cancelled()
    }
    fn recover_storage(&self) -> Result<(), crate::session_store::RecordStoreError> {
        self.inner.recover_storage()
    }
    fn storage_unavailable(&self) -> bool {
        self.faults.storage_unavailable() || self.inner.storage_unavailable()
    }

    fn storage_admission(&self) -> crate::session_store::StorageAdmission {
        self.faults
            .storage_admission()
            .unwrap_or_else(|| self.inner.storage_admission())
    }

    fn emit_projected_turn_usages(&self, usages: &[sessions::projector::ProjectedTurnUsage]) {
        self.inner.emit_projected_turn_usages(usages);
    }
    fn emit_projected_abandonments(
        &self,
        abandonments: &[sessions::projector::ProjectedAbandonment],
    ) {
        self.inner.emit_projected_abandonments(abandonments);
    }
    fn write_host_controls(
        &self,
        id: &str,
        suppress_wake: bool,
        pause: Option<sessions::projector::HostPauseCause>,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        self.faults
            .before_write(StoreCall::WriteHostControls, None)?;
        self.inner.write_host_controls(id, suppress_wake, pause)
    }

    fn write_root_loss(
        &self,
        id: &str,
        intent: sessions::projector::RootLossIntent,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        self.faults.before_write(StoreCall::WriteRootLoss, None)?;
        self.inner.write_root_loss(id, intent)
    }

    fn load(&self, id: &str) -> Result<SessionRecord, String> {
        self.faults.before(StoreCall::Load, None)?;
        self.inner.load(id)
    }

    fn load_queue_head(&self, id: &str) -> Result<crate::session_store::SessionQueueHead, String> {
        self.faults.before(StoreCall::LoadQueueHead, None)?;
        self.inner.load_queue_head(id)
    }

    fn save(
        &self,
        session: &mut SessionRecord,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        self.faults.before_write(StoreCall::Save, Some(session))?;
        self.inner.save(session)
    }

    fn save_submission(
        &self,
        session: &mut SessionRecord,
        submission: &sessions::projector::Submission,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        self.faults
            .before_write(StoreCall::SaveSubmission, Some(session))?;
        self.inner.save_submission(session, submission)
    }

    fn discard_submission(
        &self,
        session: &mut SessionRecord,
        command_id: &str,
        pause_cause: Option<sessions::projector::HostPauseCause>,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        self.faults
            .before_write(StoreCall::DiscardSubmission, Some(session))?;
        self.inner
            .discard_submission(session, command_id, pause_cause)
    }

    fn submit_workflow_request(
        &self,
        session: &mut SessionRecord,
        request: &sessions::projector::WorkflowRequest,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        self.faults
            .before_write(StoreCall::SubmitWorkflowRequest, Some(session))?;
        self.inner.submit_workflow_request(session, request)
    }

    fn discard_unadmitted_workflow(
        &self,
        session: &mut SessionRecord,
        operation_id: &str,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        self.faults
            .before_write(StoreCall::DiscardUnadmittedWorkflow, Some(session))?;
        self.inner
            .discard_unadmitted_workflow(session, operation_id)
    }

    fn withdraw_unadmitted_workflows(
        &self,
        session: &mut SessionRecord,
    ) -> Result<Vec<TranscriptRow>, crate::session_store::RecordStoreError> {
        self.faults
            .before_write(StoreCall::WithdrawUnadmittedWorkflows, Some(session))?;
        self.inner.withdraw_unadmitted_workflows(session)
    }

    fn list_for_workspace(
        &self,
        workspace: &std::path::Path,
    ) -> Result<Vec<SessionRecord>, String> {
        self.faults.before(StoreCall::ListForWorkspace, None)?;
        self.inner.list_for_workspace(workspace)
    }

    fn load_intents(&self, id: &str) -> Result<sessions::projector::ProductIntents, String> {
        self.faults.before(StoreCall::LoadIntents, None)?;
        self.inner.load_intents(id)
    }

    fn load_inherited_submissions(
        &self,
        id: &str,
    ) -> Result<Vec<sessions::projector::Submission>, String> {
        self.faults
            .before(StoreCall::LoadInheritedSubmissions, None)?;
        self.inner.load_inherited_submissions(id)
    }

    fn load_views(&self, id: &str) -> Result<Option<sessions::projector::SessionViews>, String> {
        self.faults.before(StoreCall::LoadViews, None)?;
        self.inner.load_views(id)
    }

    fn load_queue_commands(&self, id: &str) -> Result<sessions::QueueCommands, String> {
        self.faults.before(StoreCall::LoadQueueCommands, None)?;
        self.inner.load_queue_commands(id)
    }

    fn commit_projection_views(
        &self,
        id: &str,
        writes: &sessions::projector::ViewWrites,
        views: &sessions::projector::SessionViewState,
    ) -> Result<sessions::projector::ProjectionCommit, crate::session_store::RecordStoreError> {
        self.faults
            .before_write(StoreCall::CommitProjectionViews, None)?;
        self.inner.commit_projection_views(id, writes, views)
    }
}

#[test]
fn fault_store_delegates_cas_and_reads_without_swallowing_refusals() {
    use super::test_support::SaveFailure;
    use std::sync::atomic::{AtomicBool, Ordering};

    let inner = Arc::new(crate::session_store::MemoryStore::new());
    let store = FaultStore::new(
        inner.clone(),
        SaveFailure {
            fail: AtomicBool::new(false),
        },
    );
    let mut session = SessionRecord::new(std::path::PathBuf::from("workspace"));
    store.save(&mut session).expect("delegated save");
    let saved = session.clone();
    assert_eq!(store.load(&session.id).expect("delegated read"), saved);
    store.faults.fail.store(true, Ordering::SeqCst);
    assert!(store.save(&mut session).is_err());
    assert_eq!(session, saved);
    assert_eq!(
        inner.load(&session.id).expect("unchanged inner store"),
        saved
    );
    store.faults.fail.store(false, Ordering::SeqCst);
    store.save(&mut session).expect("retry delegates");
    assert!(session.store_revision > saved.store_revision);
    assert!(
        store.save(&mut saved.clone()).is_err(),
        "delegate retains CAS refusal"
    );
}
