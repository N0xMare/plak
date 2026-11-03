//! The runner arm of a process-session service: an environment whose
//! operations run on another runner keeps this service for its job
//! bookkeeping, and every process operation goes to one line's sessions on
//! that runner (spec 01 §8.5). The local arm never reaches this module.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use super::{
    ExecutionBackendError, JoinedProcessCursorPollResult, JoinedProcessInputDisposition,
    JoinedProcessOutputCursor, JoinedProcessPollResult, JoinedProcessServiceInner,
    JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessStartRequest, JoinedProcessStatus,
    OperationContext, conflict_error, lock,
};
use crate::environment::runner::{RunnerSessions, wait_on};
use crate::internal::{ProcessQuietPollState, ProcessSessionProjectionState};
use crate::{JobStatus, JobStopReason, JobView};

/// How long one background wait for a session's output lasts.
const CHANGE_WAIT: Duration = Duration::from_secs(1);
/// How long a stop may wait for its process group, as a local stop.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// A bounded read for the background change wait.
const CHANGE_BYTES: usize = 64 * 1024;

/// One session this service started on the runner.
struct ArmSession {
    status: JoinedProcessStatus,
    promoted: bool,
    projection: ProcessSessionProjectionState,
    changes: tokio::sync::watch::Sender<u64>,
    watching: bool,
}

impl ArmSession {
    fn new() -> Self {
        Self {
            status: JoinedProcessStatus::Running,
            promoted: false,
            projection: (
                Arc::new(Mutex::new(JoinedProcessOutputCursor::new(0))),
                Arc::new(Mutex::new(Vec::new())),
                Arc::new(Mutex::new(ProcessQuietPollState::default())),
            ),
            changes: tokio::sync::watch::channel(0).0,
            watching: false,
        }
    }
}

#[derive(Default)]
struct ArmState {
    sessions: BTreeMap<JoinedProcessSessionId, ArmSession>,
    /// Job numbers the agent consumed: still listed by the runner, never
    /// reported again here.
    consumed: BTreeSet<u32>,
    /// Session identities of jobs already reported running to the observer.
    observed: BTreeSet<[u8; 16]>,
    /// A destructive poll's cursor per session.
    polled: BTreeMap<JoinedProcessSessionId, u64>,
}

/// One line's sessions on a runner, behind this process's service.
pub(in crate::environment::backend) struct RunnerArm {
    sessions: Arc<RunnerSessions>,
    runtime: tokio::runtime::Handle,
    state: Mutex<ArmState>,
}

impl RunnerArm {
    fn wait<T: Send>(
        &self,
        effect: impl Future<Output = Result<T, ExecutionBackendError>> + Send,
    ) -> Result<T, ExecutionBackendError> {
        wait_on(&self.runtime, effect)?
    }

    fn control() -> OperationContext {
        OperationContext::new(plakous_model::CancellationToken::new())
    }

    fn update(&self, id: JoinedProcessSessionId, status: JoinedProcessStatus) {
        let mut state = lock(&self.state);
        let session = state.sessions.entry(id).or_insert_with(ArmSession::new);
        if session.status != status {
            session.status = status;
            session
                .changes
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    fn status(&self, id: JoinedProcessSessionId) -> Option<JoinedProcessStatus> {
        lock(&self.state)
            .sessions
            .get(&id)
            .map(|session| session.status)
    }
}

impl JoinedProcessSessions {
    /// The service of an environment whose operations run on another
    /// runner, over that runner's sessions of the environment's line, whose
    /// link runs on `runtime`. It reads the line's job events from the
    /// runner for as long as it lives.
    pub(crate) fn runner(
        environment: &crate::ExecutionEnvironment,
        sessions: RunnerSessions,
        runtime: &tokio::runtime::Handle,
    ) -> Result<Self, ExecutionBackendError> {
        let service = Self::refusing(environment)?;
        let arm = Arc::new(RunnerArm {
            sessions: Arc::new(sessions),
            runtime: runtime.clone(),
            state: Mutex::default(),
        });
        let _ = service.inner.runner.set(Arc::clone(&arm));
        let inner = Arc::downgrade(&service.inner);
        runtime.spawn(watch_jobs(Arc::downgrade(&arm), inner));
        Ok(service)
    }

    pub(super) fn runner_arm(&self) -> Option<&Arc<RunnerArm>> {
        self.inner.runner.get()
    }
}

/// Follows the line's job events while the service lives, reopening the
/// stream after a lost link.
async fn watch_jobs(arm: Weak<RunnerArm>, inner: Weak<JoinedProcessServiceInner>) {
    loop {
        let Some(sessions) = arm.upgrade().map(|arm| Arc::clone(&arm.sessions)) else {
            return;
        };
        let events = {
            let arm = arm.clone();
            let inner = inner.clone();
            move |jobs: Vec<JobView>| {
                if let (Some(arm), Some(inner)) = (arm.upgrade(), inner.upgrade()) {
                    arm.on_jobs(&JoinedProcessSessions { inner }, &jobs);
                }
            }
        };
        let _ = sessions.watch(events).await;
        if inner.upgrade().is_none() {
            return;
        }
        tokio::time::sleep(CHANGE_WAIT).await;
    }
}

impl RunnerArm {
    /// Applies one report of the line's jobs: each newly running job is
    /// reported to the host's observer, and each newly ended one joins the
    /// service's terminal notices and wakes their subscribers.
    fn on_jobs(&self, service: &JoinedProcessSessions, jobs: &[JobView]) {
        let observer = lock(&service.inner.job_observer).clone();
        let mut changed = false;
        for job in jobs {
            let id = JoinedProcessSessionId(job.session_id);
            let status = match job.status {
                JobStatus::Running => JoinedProcessStatus::Running,
                JobStatus::Exited(code) => JoinedProcessStatus::Exited(code),
                JobStatus::Released => JoinedProcessStatus::Released,
                JobStatus::Stopped(_) => JoinedProcessStatus::Terminated,
                JobStatus::OutcomeUnknown => JoinedProcessStatus::OutcomeUnknown,
            };
            {
                let mut state = lock(&self.state);
                if let Some(session) = state.sessions.get_mut(&id) {
                    session.promoted = true;
                }
                if state.consumed.contains(&job.number) {
                    continue;
                }
                let first = state.observed.insert(job.session_id);
                drop(state);
                if first && let Some(observer) = &observer {
                    let mut running = job.clone();
                    running.status = JobStatus::Running;
                    let _ = (observer.record)(running);
                }
            }
            if lock(&self.state).sessions.contains_key(&id) {
                self.update(id, status);
            }
            if job.status != JobStatus::Running {
                let mut state = lock(&service.inner.state);
                if state.terminal_membership.insert(id) {
                    state.terminal_jobs.push(id);
                    drop(state);
                    if let Some(observer) = &observer {
                        let _ = (observer.record)(job.clone());
                    }
                    changed = true;
                }
            }
        }
        if changed {
            service
                .inner
                .terminal_changes
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    /// The runner's jobs this service has not consumed.
    pub(super) fn jobs(&self) -> Vec<JobView> {
        let consumed = lock(&self.state).consumed.clone();
        self.sessions
            .cached_jobs()
            .into_iter()
            .filter(|job| !consumed.contains(&job.number))
            .collect()
    }

    pub(super) fn consume(&self, numbers: &[u32]) {
        lock(&self.state).consumed.extend(numbers.iter().copied());
    }

    pub(super) fn has_unreported_terminal_job(&self) -> bool {
        self.jobs()
            .iter()
            .any(|job| job.status != JobStatus::Running)
    }

    pub(super) fn running_job_count(&self) -> usize {
        self.jobs()
            .iter()
            .filter(|job| job.status == JobStatus::Running)
            .count()
    }

    /// A session's number is its id's, as for the local service; a job this
    /// service did not start is found through the line's job events.
    pub(super) fn session_for_number(&self, number: u32) -> Option<JoinedProcessSessionId> {
        let known = lock(&self.state)
            .sessions
            .keys()
            .find(|id| JoinedProcessSessions::job_number(**id) == number)
            .copied();
        known.or_else(|| self.sessions.session_for_number(number))
    }

    pub(super) fn start(
        &self,
        request: JoinedProcessStartRequest,
        control: OperationContext,
    ) -> Result<JoinedProcessSessionId, ExecutionBackendError> {
        let id = self.wait(self.sessions.start(request, control))?;
        lock(&self.state).sessions.insert(id, ArmSession::new());
        Ok(id)
    }

    pub(super) fn write_stdin(
        &self,
        id: JoinedProcessSessionId,
        bytes: &[u8],
        disposition: JoinedProcessInputDisposition,
    ) -> Result<(), ExecutionBackendError> {
        self.wait(
            self.sessions
                .write_stdin(id, bytes, disposition, Self::control()),
        )
    }

    pub(super) fn poll_from(
        &self,
        id: JoinedProcessSessionId,
        cursor: JoinedProcessOutputCursor,
        max_bytes: usize,
        max_records: usize,
    ) -> Result<JoinedProcessCursorPollResult, ExecutionBackendError> {
        let result = self.wait(self.sessions.poll_from(
            id,
            cursor,
            max_bytes,
            max_records,
            Duration::ZERO,
            Self::control(),
        ))?;
        self.update(id, result.status());
        Ok(result)
    }

    /// A destructive poll over this service's own cursor for the session.
    pub(super) fn poll(
        &self,
        id: JoinedProcessSessionId,
        max_bytes: usize,
        max_records: usize,
    ) -> Result<JoinedProcessPollResult, ExecutionBackendError> {
        let cursor = lock(&self.state).polled.get(&id).copied().unwrap_or(0);
        let result = self.poll_from(
            id,
            JoinedProcessOutputCursor::new(cursor),
            max_bytes,
            max_records,
        )?;
        lock(&self.state)
            .polled
            .insert(id, result.next_cursor().get());
        Ok(JoinedProcessPollResult {
            records: result.records.clone(),
            status: result.status(),
            more_output: result.more_output,
        })
    }

    /// The latest output within the bounds: the window from the oldest
    /// retained record, so a caller sees what the session kept.
    pub(super) fn poll_tail(
        &self,
        id: JoinedProcessSessionId,
        max_bytes: usize,
        max_records: usize,
    ) -> Result<JoinedProcessCursorPollResult, ExecutionBackendError> {
        self.poll_from(
            id,
            JoinedProcessOutputCursor::new(0),
            max_bytes,
            max_records,
        )
    }

    pub(super) fn projection_state(
        &self,
        id: JoinedProcessSessionId,
    ) -> ProcessSessionProjectionState {
        let mut state = lock(&self.state);
        let session = state.sessions.entry(id).or_insert_with(ArmSession::new);
        (
            Arc::clone(&session.projection.0),
            Arc::clone(&session.projection.1),
            Arc::clone(&session.projection.2),
        )
    }

    /// Subscribes to the session's output and status, which a background
    /// wait on the runner keeps current.
    pub(super) fn session_changes(
        self: &Arc<Self>,
        id: JoinedProcessSessionId,
    ) -> tokio::sync::watch::Receiver<u64> {
        let mut state = lock(&self.state);
        let session = state.sessions.entry(id).or_insert_with(ArmSession::new);
        let receiver = session.changes.subscribe();
        if !session.watching {
            session.watching = true;
            let arm = Arc::downgrade(self);
            self.runtime.spawn(watch_session(arm, id));
        }
        receiver
    }

    pub(super) fn joined_terminal_status(
        &self,
        id: JoinedProcessSessionId,
    ) -> Option<JoinedProcessStatus> {
        self.status(id).filter(|status| status.is_terminal())
    }

    pub(super) fn active_session_ids(&self) -> Vec<JoinedProcessSessionId> {
        lock(&self.state)
            .sessions
            .iter()
            .filter(|(_, session)| !session.status.is_terminal())
            .map(|(id, _)| *id)
            .collect()
    }

    pub(super) fn is_promoted(&self, id: JoinedProcessSessionId) -> bool {
        lock(&self.state)
            .sessions
            .get(&id)
            .is_some_and(|session| session.promoted)
    }

    pub(super) fn knows(&self, id: JoinedProcessSessionId) -> bool {
        lock(&self.state).sessions.contains_key(&id)
    }

    pub(super) fn stop_with_grace(
        &self,
        id: JoinedProcessSessionId,
        by: JobStopReason,
        grace: Duration,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        let status = self.wait(self.sessions.stop_with_grace(id, by, grace))?;
        self.update(id, status);
        Ok(status)
    }

    pub(super) fn terminate(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        self.stop_with_grace(id, JobStopReason::Model, STOP_GRACE)
    }

    pub(super) fn join(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        let polled = self.poll_from(id, JoinedProcessOutputCursor::new(0), 1, 1)?;
        if !polled.status().is_terminal() {
            return Err(conflict_error("process is still running"));
        }
        Ok(polled.status())
    }

    pub(super) fn promote(&self, id: JoinedProcessSessionId) -> Result<(), ExecutionBackendError> {
        self.wait(self.sessions.promote(id))?;
        if let Some(session) = lock(&self.state).sessions.get_mut(&id) {
            session.promoted = true;
        }
        self.wait(self.sessions.jobs()).map(drop)
    }

    pub(super) fn stop_jobs(
        &self,
        by: JobStopReason,
        grace: Duration,
    ) -> Result<(), ExecutionBackendError> {
        self.wait(self.sessions.stop_jobs(by, grace))
    }

    /// Stops the sessions it started that are not jobs: what a local
    /// service's foreground stop and shutdown join stop. Jobs stay on the
    /// runner.
    pub(super) fn stop_foreground(&self) -> Result<(), ExecutionBackendError> {
        let foreground = lock(&self.state)
            .sessions
            .iter()
            .filter(|(_, session)| !session.promoted && !session.status.is_terminal())
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let mut failure = None;
        for id in foreground {
            if let Err(error) = self.terminate(id) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    pub(super) fn disown_leftovers(&self) -> Result<(), ExecutionBackendError> {
        self.wait(self.sessions.disown_leftovers())
    }
}

/// Waits on the runner for each new piece of a session's output or its end,
/// and wakes the session's subscribers, until it ends or the service goes.
async fn watch_session(arm: Weak<RunnerArm>, id: JoinedProcessSessionId) {
    let mut cursor = 0;
    loop {
        let Some(strong) = arm.upgrade() else {
            return;
        };
        let sessions = Arc::clone(&strong.sessions);
        drop(strong);
        let polled = sessions
            .poll_from(
                id,
                JoinedProcessOutputCursor::new(cursor),
                CHANGE_BYTES,
                64,
                CHANGE_WAIT,
                RunnerArm::control(),
            )
            .await;
        let Some(strong) = arm.upgrade() else {
            return;
        };
        let Ok(polled) = polled else {
            tokio::time::sleep(CHANGE_WAIT).await;
            continue;
        };
        let advanced = polled.next_cursor().get() != cursor;
        cursor = polled.next_cursor().get();
        let terminal = polled.status().is_terminal();
        let mut state = lock(&strong.state);
        if let Some(session) = state.sessions.get_mut(&id) {
            if advanced || session.status != polled.status() {
                session.status = polled.status();
                session
                    .changes
                    .send_modify(|revision| *revision = revision.wrapping_add(1));
            }
            if terminal {
                session.watching = false;
                return;
            }
        } else {
            return;
        }
    }
}
