//! One agent line's process sessions on a runner: the host handle that sends
//! the session verbs, and the runner side that answers them from that line's
//! own [`JoinedProcessSessions`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_model::CancellationToken;
use serde_json::{Value, json};

use super::super::{
    ExecutionBackendError, ExecutionWorkspace, JoinedProcessCursorPollResult,
    JoinedProcessHostCapacity, JoinedProcessInputDisposition, JoinedProcessOutputCursor,
    JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessStartRequest, JoinedProcessStatus,
    LineOwner, OperationContext,
};
use super::sessions_wire::{
    PollWire, SessionWire, StartedWire, StopWire, WriteWire, decode_polled, decode_reason,
    decode_session, decode_snapshot, decode_start, decode_status, encode_polled, encode_reason,
    encode_session, encode_snapshot, encode_start, encode_status,
};
use super::wire::{self, from_value, hex, to_value, unhex};
use super::{
    RunnerBackend, RunnerCall, RunnerClass, RunnerLink, RunnerVerb, RunnerVerbFamily, ServeFailure,
    ServeFuture, backend_failure, failure_error,
};
use crate::{ExecutionEnvironment, JobStatus, JobStopReason, JobView, ToolPreempt};

/// How often a waiting poll looks for output.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// A host handle on one agent line's process sessions on a runner. Every
/// call names the handle's owner, so the runner fences it and keeps the
/// line's jobs across the host's owner changes; a read of the line's jobs
/// marks every job a restarted runner no longer holds as of unknown outcome.
pub struct RunnerSessions {
    link: Arc<dyn RunnerLink>,
    workspace: ExecutionWorkspace,
    owner: Mutex<LineOwner>,
    mirror: Mutex<Mirror>,
    /// Bumped whenever a read of the line's jobs finds one newly ended.
    terminal_changes: tokio::sync::watch::Sender<u64>,
}

/// The jobs this handle has seen, by the runner instance that held them and
/// their number there: a restarted runner numbers its jobs afresh.
#[derive(Default)]
struct Mirror {
    /// Each instance's jobs, oldest instance first; the last is current.
    instances: Vec<(String, BTreeMap<u32, JobView>)>,
}

impl Mirror {
    /// The jobs of the instance a result came from. Jobs an earlier
    /// instance held that had not ended are of unknown outcome: the restart
    /// lost them.
    fn observe(&mut self, instance: &str) -> &mut BTreeMap<u32, JobView> {
        let known = self
            .instances
            .last()
            .is_some_and(|(current, _)| current == instance);
        if !known {
            for (_, jobs) in &mut self.instances {
                for job in jobs.values_mut() {
                    if job.status == JobStatus::Running {
                        job.status = JobStatus::OutcomeUnknown;
                    }
                }
            }
            self.instances.push((instance.to_owned(), BTreeMap::new()));
        }
        let index = self.instances.len() - 1;
        &mut self.instances[index].1
    }

    fn jobs(&self) -> Vec<JobView> {
        self.instances
            .iter()
            .flat_map(|(_, jobs)| jobs.values().cloned())
            .collect()
    }
}

impl std::fmt::Debug for RunnerSessions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerSessions")
            .finish_non_exhaustive()
    }
}

impl RunnerSessions {
    /// The sessions of `owner`'s line on `backend`'s runner, over
    /// `environment`'s workspace.
    #[must_use]
    pub fn new(
        backend: &RunnerBackend,
        environment: &ExecutionEnvironment,
        owner: LineOwner,
    ) -> Self {
        Self {
            link: backend.link(),
            workspace: ExecutionWorkspace::from_environment(environment),
            owner: Mutex::new(owner),
            mirror: Mutex::default(),
            terminal_changes: tokio::sync::watch::channel(0).0,
        }
    }

    /// The line head this handle acts for.
    #[must_use]
    pub fn owner(&self) -> LineOwner {
        *lock(&self.owner)
    }

    /// Hands the line to a newer head: its next call transfers the line on
    /// the runner first, and the line's jobs stay there.
    pub fn transfer_to(&self, owner: LineOwner) {
        *lock(&self.owner) = owner;
    }

    fn control(&self, control: &OperationContext) -> OperationContext {
        control.clone().with_owner(self.owner())
    }

    fn controlled(&self) -> OperationContext {
        OperationContext::new(CancellationToken::new()).with_owner(self.owner())
    }

    async fn send(
        &self,
        verb: RunnerVerb,
        payload: Value,
        control: &OperationContext,
        class: Option<RunnerClass>,
    ) -> Result<Value, ExecutionBackendError> {
        let mut call = RunnerCall::new(verb, payload, &self.control(control));
        if let Some(class) = class {
            call = call.with_class(class);
        }
        self.link.call(call).await.map_err(failure_error)
    }

    /// Starts one process under the caller's key.
    ///
    /// # Errors
    ///
    /// The runner's start error, or the link's failure.
    pub async fn start(
        &self,
        request: JoinedProcessStartRequest,
        control: OperationContext,
    ) -> Result<JoinedProcessSessionId, ExecutionBackendError> {
        let payload = wire::envelope(&self.workspace, encode_start(&request)?)?;
        let started: StartedWire = from_value(
            self.send(RunnerVerb::ProcessStart, payload, &control, None)
                .await?,
        )?;
        lock(&self.mirror).observe(&started.instance);
        decode_session(&started.session)
    }

    /// Writes to a session's stdin, and closes it when asked.
    ///
    /// # Errors
    ///
    /// The runner's write error, or the link's failure.
    pub async fn write_stdin(
        &self,
        id: JoinedProcessSessionId,
        bytes: &[u8],
        disposition: JoinedProcessInputDisposition,
        control: OperationContext,
    ) -> Result<(), ExecutionBackendError> {
        let payload = to_value(&WriteWire {
            session: encode_session(id),
            bytes: hex(bytes),
            close: disposition == JoinedProcessInputDisposition::Close,
        })?;
        self.send(RunnerVerb::ProcessWrite, payload, &control, None)
            .await
            .map(drop)
    }

    /// Output from `cursor`, waiting up to `wait` for some to arrive or for
    /// the process to end. A yield on the caller's control ends the wait.
    ///
    /// # Errors
    ///
    /// The runner's poll error, or the link's failure.
    pub async fn poll_from(
        &self,
        id: JoinedProcessSessionId,
        cursor: JoinedProcessOutputCursor,
        max_bytes: usize,
        max_records: usize,
        wait: Duration,
        control: OperationContext,
    ) -> Result<JoinedProcessCursorPollResult, ExecutionBackendError> {
        let payload = to_value(&PollWire {
            session: encode_session(id),
            cursor: cursor.get(),
            max_bytes,
            max_records,
            wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
        })?;
        let class = RunnerClass::Observation {
            job: JoinedProcessSessions::job_number(id),
            cursor: cursor.get(),
        };
        decode_polled(
            self.send(RunnerVerb::ProcessPoll, payload, &control, Some(class))
                .await?,
        )
    }

    /// Stops one session with a bounded grace.
    ///
    /// # Errors
    ///
    /// The runner's stop error, or the link's failure.
    pub async fn stop_with_grace(
        &self,
        id: JoinedProcessSessionId,
        by: JobStopReason,
        grace: Duration,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        let payload = stop(Some(id), by, grace)?;
        let status = self
            .send(
                RunnerVerb::ProcessStop,
                payload,
                &self.controlled(),
                Some(RunnerClass::Control),
            )
            .await?;
        decode_status(&status)
    }

    /// Promotes a foreground session to a job of the line.
    ///
    /// # Errors
    ///
    /// The runner's promotion refusal, or the link's failure.
    pub async fn promote(&self, id: JoinedProcessSessionId) -> Result<(), ExecutionBackendError> {
        let payload = to_value(&SessionWire {
            session: encode_session(id),
        })?;
        self.send(
            RunnerVerb::ProcessPromote,
            payload,
            &self.controlled(),
            Some(RunnerClass::Control),
        )
        .await
        .map(drop)
    }

    /// The line's jobs: every job this handle saw on an earlier runner
    /// instance, as of unknown outcome unless it had ended, then the current
    /// instance's view. Numbers repeat across instances.
    ///
    /// # Errors
    ///
    /// The link's failure.
    pub async fn jobs(&self) -> Result<Vec<JobView>, ExecutionBackendError> {
        let snapshot = self
            .send(
                RunnerVerb::JobsSnapshot,
                json!({}),
                &self.controlled(),
                Some(RunnerClass::Unkeyed),
            )
            .await?;
        let (instance, jobs) = decode_snapshot(snapshot)?;
        let mut mirror = lock(&self.mirror);
        let ended = |jobs: &[JobView]| {
            jobs.iter()
                .filter(|job| job.status != JobStatus::Running)
                .count()
        };
        let before = ended(&mirror.jobs());
        let current = mirror.observe(&instance);
        for job in jobs {
            current.insert(job.number, job);
        }
        let jobs = mirror.jobs();
        if ended(&jobs) != before {
            self.terminal_changes
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
        Ok(jobs)
    }

    /// The line's jobs as the last read found them, without asking the
    /// runner.
    #[must_use]
    pub fn cached_jobs(&self) -> Vec<JobView> {
        lock(&self.mirror).jobs()
    }

    /// The revision [`Self::jobs`] bumps when it finds a job newly ended.
    #[must_use]
    pub fn terminal_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.terminal_changes.subscribe()
    }

    /// The session of the current instance's job `number`, when a read saw
    /// it.
    #[must_use]
    pub fn session_for_number(&self, number: u32) -> Option<JoinedProcessSessionId> {
        let mirror = lock(&self.mirror);
        let (_, jobs) = mirror.instances.last()?;
        jobs.get(&number)
            .map(|job| JoinedProcessSessionId::from_runtime_bytes(job.session_id))
    }

    /// Stops every job of the line.
    ///
    /// # Errors
    ///
    /// The runner's stop error, or the link's failure.
    pub async fn stop_jobs(
        &self,
        by: JobStopReason,
        grace: Duration,
    ) -> Result<(), ExecutionBackendError> {
        let payload = stop(None, by, grace)?;
        self.send(
            RunnerVerb::JobsStop,
            payload,
            &self.controlled(),
            Some(RunnerClass::Control),
        )
        .await
        .map(drop)
    }

    /// Disowns the line's leftover processes on the runner.
    ///
    /// # Errors
    ///
    /// The runner's disown error, or the link's failure.
    pub async fn disown_leftovers(&self) -> Result<(), ExecutionBackendError> {
        self.send(
            RunnerVerb::JobsDisown,
            json!({}),
            &self.controlled(),
            Some(RunnerClass::Control),
        )
        .await
        .map(drop)
    }
}

fn stop(
    id: Option<JoinedProcessSessionId>,
    by: JobStopReason,
    grace: Duration,
) -> Result<Value, ExecutionBackendError> {
    to_value(&StopWire {
        session: id.map(encode_session),
        reason: encode_reason(by).to_owned(),
        grace_ms: u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A shared bound of `limit` running processes across every line a runner
/// serves: the job bound its hello announces.
#[must_use]
pub fn job_capacity(limit: usize) -> JoinedProcessHostCapacity {
    JoinedProcessHostCapacity::with_limit(limit)
}

/// A new process-session service for one line on a runner, counted against
/// the runner's `capacity`.
///
/// # Errors
///
/// Refuses an environment that is not a host environment.
pub fn line_sessions(
    environment: &ExecutionEnvironment,
    capacity: &JoinedProcessHostCapacity,
) -> Result<JoinedProcessSessions, ExecutionBackendError> {
    let sessions = JoinedProcessSessions::new(environment)?;
    sessions.bind_host_capacity(capacity.clone());
    Ok(sessions)
}

/// Answers one session verb from a line's `sessions`, as the runner side of
/// a link; `instance` names this runner process in starts and snapshots. A
/// verb of another family is [`ServeFailure::Unsupported`].
pub fn serve_sessions(
    environment: &ExecutionEnvironment,
    sessions: &JoinedProcessSessions,
    instance: &str,
    verb: &str,
    payload: Value,
    control: OperationContext,
) -> ServeFuture {
    let Some(verb) =
        RunnerVerb::parse(verb).filter(|verb| verb.family() == RunnerVerbFamily::Sessions)
    else {
        return Box::pin(std::future::ready(Err(ServeFailure::Unsupported)));
    };
    let environment = environment.clone();
    let sessions = sessions.clone();
    let instance = instance.to_owned();
    Box::pin(async move {
        let answer = match verb {
            RunnerVerb::ProcessPoll => poll(&sessions, payload, &control).await,
            RunnerVerb::JobsSnapshot => encode_snapshot(&instance, &sessions.jobs()),
            _ => {
                // Starts, writes, stops and promotions block on the child.
                tokio::task::spawn_blocking(move || {
                    effect(&environment, &sessions, &instance, verb, payload, control)
                })
                .await
                .unwrap_or_else(|_| Err(wire::malformed()))
            }
        };
        answer.map_err(|error| backend_failure(&error))
    })
}

fn effect(
    environment: &ExecutionEnvironment,
    sessions: &JoinedProcessSessions,
    instance: &str,
    verb: RunnerVerb,
    payload: Value,
    control: OperationContext,
) -> Result<Value, ExecutionBackendError> {
    match verb {
        RunnerVerb::ProcessStart => {
            let request = decode_start(wire::open_envelope(environment, payload)?)?;
            let id = sessions.start(request, control)?;
            to_value(&StartedWire {
                session: encode_session(id),
                instance: instance.to_owned(),
            })
        }
        RunnerVerb::ProcessWrite => {
            let write: WriteWire = from_value(payload)?;
            let disposition = if write.close {
                JoinedProcessInputDisposition::Close
            } else {
                JoinedProcessInputDisposition::KeepOpen
            };
            sessions.write_stdin(
                decode_session(&write.session)?,
                &unhex(&write.bytes)?,
                disposition,
            )?;
            Ok(json!({}))
        }
        RunnerVerb::ProcessStop | RunnerVerb::JobsStop => {
            let stop: StopWire = from_value(payload)?;
            let by = decode_reason(&stop.reason)?;
            let grace = Duration::from_millis(stop.grace_ms);
            match stop.session {
                Some(session) => sessions
                    .stop_with_grace(decode_session(&session)?, by, grace)
                    .map(encode_status),
                None => sessions.stop_jobs(by, grace).map(|()| json!({})),
            }
        }
        RunnerVerb::ProcessPromote => {
            let session: SessionWire = from_value(payload)?;
            sessions.promote(decode_session(&session.session)?)?;
            Ok(json!({}))
        }
        _ => {
            sessions.disown_leftovers()?;
            Ok(json!({}))
        }
    }
}

async fn poll(
    sessions: &JoinedProcessSessions,
    payload: Value,
    control: &OperationContext,
) -> Result<Value, ExecutionBackendError> {
    let poll: PollWire = from_value(payload)?;
    let id = decode_session(&poll.session)?;
    let deadline = Instant::now() + Duration::from_millis(poll.wait_ms);
    let preempt = control.preempt().clone();
    loop {
        let result = sessions.poll_from(
            id,
            JoinedProcessOutputCursor::new(poll.cursor),
            poll.max_bytes,
            poll.max_records,
        )?;
        let settled = !result.records().is_empty()
            || result.status().is_terminal()
            || Instant::now() >= deadline
            || *preempt.borrow() != ToolPreempt::None
            || control.cancellation().is_cancelled();
        if settled {
            return encode_polled(&result);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
