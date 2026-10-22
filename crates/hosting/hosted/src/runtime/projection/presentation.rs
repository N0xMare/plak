//! Process-local presentation follows every winning store projection.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::session_store::SessionRecordStore;
use crate::sessions::SessionRecord;
use crate::sessions::projector::{
    ProductIntents, ProjectedTurnUsage, ProjectionCommit, SessionViews, Submission, ViewWrites,
};
use plakous::model::{SessionId, TurnId};

#[derive(Default)]
pub(crate) struct ProcessUsage {
    pub(crate) target: Option<SessionId>,
    seen: std::collections::HashSet<TurnId>,
    pub(crate) pending: Vec<ProjectedTurnUsage>,
}

pub(crate) struct PresentingStore {
    pub(crate) inner: Arc<dyn SessionRecordStore>,
    pub(crate) usage: Arc<Mutex<ProcessUsage>>,
}

impl SessionRecordStore for PresentingStore {
    fn load(&self, id: &str) -> Result<SessionRecord, String> {
        self.inner.load(id)
    }

    fn load_queue_head(&self, id: &str) -> Result<crate::session_store::SessionQueueHead, String> {
        self.inner.load_queue_head(id)
    }

    fn save(&self, session: &mut SessionRecord) -> Result<(), String> {
        self.inner.save(session)
    }

    fn write_host_controls(
        &self,
        id: &str,
        suppress_wake: bool,
        pause: Option<crate::sessions::projector::HostPauseCause>,
    ) -> Result<(), String> {
        self.inner.write_host_controls(id, suppress_wake, pause)
    }

    fn save_submission(
        &self,
        session: &mut SessionRecord,
        submission: &Submission,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        self.inner.save_submission(session, submission)
    }

    fn discard_submission(
        &self,
        session: &mut SessionRecord,
        command_id: &str,
        pause_cause: Option<crate::sessions::projector::HostPauseCause>,
    ) -> Result<(), String> {
        self.inner
            .discard_submission(session, command_id, pause_cause)
    }

    fn submit_workflow_request(
        &self,
        session: &mut SessionRecord,
        request: &crate::sessions::projector::WorkflowRequest,
    ) -> Result<(), String> {
        self.inner.submit_workflow_request(session, request)
    }

    fn discard_unadmitted_workflow(
        &self,
        session: &mut SessionRecord,
        operation_id: &str,
    ) -> Result<(), String> {
        self.inner
            .discard_unadmitted_workflow(session, operation_id)
    }

    fn withdraw_unadmitted_workflows(
        &self,
        session: &mut SessionRecord,
    ) -> Result<Vec<crate::sessions::TranscriptRow>, String> {
        self.inner.withdraw_unadmitted_workflows(session)
    }

    fn list_for_workspace(&self, workspace: &Path) -> Result<Vec<SessionRecord>, String> {
        self.inner.list_for_workspace(workspace)
    }

    fn load_intents(&self, id: &str) -> Result<ProductIntents, String> {
        self.inner.load_intents(id)
    }

    fn load_inherited_submissions(&self, id: &str) -> Result<Vec<Submission>, String> {
        self.inner.load_inherited_submissions(id)
    }

    fn load_views(&self, id: &str) -> Result<Option<SessionViews>, String> {
        self.inner.load_views(id)
    }

    fn commit_projection_views(
        &self,
        id: &str,
        writes: &ViewWrites,
        views: &crate::sessions::projector::SessionViewState,
    ) -> Result<ProjectionCommit, String> {
        self.inner.commit_projection_views(id, writes, views)
    }
    fn emit_projected_turn_usages(&self, terminals: &[ProjectedTurnUsage]) {
        self.inner.emit_projected_turn_usages(terminals);
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for terminal in terminals {
            if usage.target == Some(terminal.session_id) && usage.seen.insert(terminal.turn_id) {
                usage.pending.push(*terminal);
            }
        }
    }
}
