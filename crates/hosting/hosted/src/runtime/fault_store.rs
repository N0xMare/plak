//! Fault policies share one complete session-record-store delegate.
use crate::session_store::SessionRecordStore;
use crate::sessions::{self, SessionRecord, TranscriptRow};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreCall {
    WriteHostControls,
    Load,
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
    CommitProjectionViews,
}

pub(crate) trait StoreFaults: Send + Sync {
    fn before(&self, call: StoreCall, session: Option<&SessionRecord>) -> Result<(), String>;
}

pub(crate) struct FaultStore<F> {
    pub(super) inner: Arc<dyn SessionRecordStore>,
    pub(crate) faults: F,
}
impl<F> FaultStore<F> {
    pub(crate) fn new(inner: Arc<dyn SessionRecordStore>, faults: F) -> Self {
        Self { inner, faults }
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

impl<F: StoreFaults> SessionRecordStore for FaultStore<F> {
    fn emit_projected_turn_usages(&self, usages: &[sessions::projector::ProjectedTurnUsage]) {
        self.inner.emit_projected_turn_usages(usages);
    }
    fn write_host_controls(
        &self,
        id: &str,
        suppress_wake: bool,
        pause: Option<sessions::projector::HostPauseCause>,
    ) -> Result<(), String> {
        self.faults.before(StoreCall::WriteHostControls, None)?;
        self.inner.write_host_controls(id, suppress_wake, pause)
    }

    fn load(&self, id: &str) -> Result<SessionRecord, String> {
        self.faults.before(StoreCall::Load, None)?;
        self.inner.load(id)
    }

    fn save(&self, session: &mut SessionRecord) -> Result<(), String> {
        self.faults.before(StoreCall::Save, Some(session))?;
        self.inner.save(session)
    }

    fn save_submission(
        &self,
        session: &mut SessionRecord,
        submission: &sessions::projector::Submission,
    ) -> Result<(), String> {
        self.faults
            .before(StoreCall::SaveSubmission, Some(session))?;
        self.inner.save_submission(session, submission)
    }

    fn discard_submission(
        &self,
        session: &mut SessionRecord,
        command_id: &str,
        pause_cause: Option<sessions::projector::HostPauseCause>,
    ) -> Result<(), String> {
        self.faults
            .before(StoreCall::DiscardSubmission, Some(session))?;
        self.inner
            .discard_submission(session, command_id, pause_cause)
    }

    fn submit_workflow_request(
        &self,
        session: &mut SessionRecord,
        request: &sessions::projector::WorkflowRequest,
    ) -> Result<(), String> {
        self.faults
            .before(StoreCall::SubmitWorkflowRequest, Some(session))?;
        self.inner.submit_workflow_request(session, request)
    }

    fn discard_unadmitted_workflow(
        &self,
        session: &mut SessionRecord,
        operation_id: &str,
    ) -> Result<(), String> {
        self.faults
            .before(StoreCall::DiscardUnadmittedWorkflow, Some(session))?;
        self.inner
            .discard_unadmitted_workflow(session, operation_id)
    }

    fn withdraw_unadmitted_workflows(
        &self,
        session: &mut SessionRecord,
    ) -> Result<Vec<TranscriptRow>, String> {
        self.faults
            .before(StoreCall::WithdrawUnadmittedWorkflows, Some(session))?;
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

    fn commit_projection_views(
        &self,
        id: &str,
        writes: &sessions::projector::ViewWrites,
        views: &sessions::projector::SessionViews,
    ) -> Result<sessions::projector::ProjectionCommit, String> {
        self.faults.before(StoreCall::CommitProjectionViews, None)?;
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
