//! One agent line's process sessions on a runner: the host handle that sends
//! the session verbs, and the runner side that answers them from that line's
//! own [`JoinedProcessSessions`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_model::CancellationToken;
use serde_json::{Value, json};

use super::super::{
    ExecutionBackendError, ExecutionWorkspace, HostProcessContext, JoinedProcessCursorPollResult,
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
/// How often a job event stream looks for a change no terminal announces.
const WATCH_TICK: Duration = Duration::from_millis(100);

/// A host handle on one agent line's process sessions on a runner. Every
/// call names the handle's owner, so the runner fences it and keeps the
/// line's jobs across the host's owner changes; a read of the line's jobs
/// marks every job a restarted runner no longer holds as of unknown outcome.
pub struct RunnerSessions {
    link: Arc<dyn RunnerLink>,
    workspace: ExecutionWorkspace,
    /// On a home-local runner, the host's own process context: each start
    /// carries what it resolves to, and the runner starts the process in it.
    local_process: Option<Arc<HostProcessContext>>,
    owner: Mutex<LineOwner>,
    mirror: Mutex<Mirror>,
    /// Bumped whenever a read of the line's jobs finds one newly ended.
    terminal_changes: tokio::sync::watch::Sender<u64>,
}

/// The jobs this handle has seen, by the runner instance that held them and
/// their number there, and the lowest number no session of the line has
/// taken that the host knows of.
#[derive(Default)]
struct Mirror {
    /// Each instance's jobs, oldest instance first; the last is current.
    instances: Vec<(String, BTreeMap<u32, JobView>)>,
    next: u32,
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

    /// Records that a session took `number`.
    fn see(&mut self, number: u32) {
        self.next = self.next.max(number.saturating_add(1));
    }

    /// The lowest number a new session may take: past every seeded and seen
    /// number, and never 0.
    fn next_number(&self) -> u32 {
        self.instances
            .iter()
            .flat_map(|(_, jobs)| jobs.keys())
            .map(|number| number.saturating_add(1))
            .fold(self.next.max(1), u32::max)
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
            local_process: None,
            owner: Mutex::new(owner),
            mirror: Mutex::default(),
            terminal_changes: tokio::sync::watch::channel(0).0,
        }
    }

    /// The sessions of `owner`'s line on the runner behind `link`, over
    /// `environment`'s workspace.
    #[must_use]
    pub fn from_link(
        link: Arc<dyn RunnerLink>,
        environment: &ExecutionEnvironment,
        owner: LineOwner,
    ) -> Self {
        Self {
            link,
            workspace: ExecutionWorkspace::from_environment(environment),
            local_process: None,
            owner: Mutex::new(owner),
            mirror: Mutex::default(),
            terminal_changes: tokio::sync::watch::channel(0).0,
        }
    }

    /// The sessions of a home-local runner, which starts each process in the
    /// context `process` resolves to when it is started.
    #[must_use]
    pub(in crate::environment) fn with_local_process(
        mut self,
        process: Arc<HostProcessContext>,
    ) -> Self {
        self.local_process = Some(process);
        self
    }

    /// Follows the line's job events until the stream ends: each report
    /// updates the mirror, and `on_jobs` receives the jobs it now holds.
    ///
    /// # Errors
    ///
    /// The stream could not be opened.
    pub async fn watch(
        &self,
        on_jobs: impl Fn(Vec<JobView>) + Send,
    ) -> Result<(), ExecutionBackendError> {
        use tokio::io::AsyncBufReadExt as _;
        // The envelope names the line's workspace, should this open the
        // line's sessions on the runner.
        let payload = wire::envelope(&self.workspace, json!({}))?;
        let call = RunnerCall::new(RunnerVerb::JobsWatch, payload, &self.controlled())
            .with_class(RunnerClass::Unkeyed);
        let (_, streams) = self.link.open(call).await.map_err(failure_error)?;
        let mut lines = tokio::io::BufReader::new(streams.output).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(snapshot) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Ok((instance, jobs)) = decode_snapshot(snapshot) else {
                continue;
            };
            let jobs = {
                let mut mirror = lock(&self.mirror);
                let current = mirror.observe(&instance);
                current.clear();
                for job in jobs {
                    current.insert(job.number, job);
                }
                mirror.jobs()
            };
            on_jobs(jobs);
        }
        Ok(())
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
        let next = self.next_job_number();
        let process = self
            .local_process
            .as_ref()
            .map(|process| process.facts())
            .transpose()?;
        let payload = wire::envelope(
            &self.workspace,
            encode_start(&request, next, process.as_ref())?,
        )?;
        let started: StartedWire = from_value(
            self.send(RunnerVerb::ProcessStart, payload, &control, None)
                .await?,
        )?;
        let id = decode_session(&started.session)?;
        let mut mirror = lock(&self.mirror);
        mirror.observe(&started.instance);
        mirror.see(JoinedProcessSessions::job_number(id));
        Ok(id)
    }

    /// Never lets a new session of the line take a number below `next`: the
    /// host's durable counter, which a runner restart does not reset.
    pub fn seed_job_numbers(&self, next: u32) {
        let mut mirror = lock(&self.mirror);
        mirror.next = mirror.next.max(next);
    }

    /// The lowest number a new session of the line may take, as far as this
    /// handle knows: past the seed and every number it has seen.
    #[must_use]
    pub fn next_job_number(&self) -> u32 {
        lock(&self.mirror).next_number()
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
                wire::envelope(&self.workspace, json!({}))?,
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

/// The line's job events as the runner side of [`RunnerVerb::JobsWatch`]
/// sends them: one line of JSON with the instance and the jobs at once, and
/// another whenever they change. The stream ends when its reader goes.
#[must_use]
pub fn serve_jobs_watch(
    sessions: &JoinedProcessSessions,
    instance: &str,
) -> std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>> {
    use tokio::io::AsyncWriteExt as _;
    let (reader, mut writer) = tokio::io::duplex(64 * 1024);
    let sessions = sessions.clone();
    let instance = instance.to_owned();
    tokio::spawn(async move {
        let mut last = None;
        let mut changes = sessions.job_terminal_changes();
        loop {
            let Ok(mut line) =
                encode_snapshot(&instance, &sessions.jobs()).map(|snapshot| snapshot.to_string())
            else {
                return;
            };
            if last.as_ref() != Some(&line) {
                last = Some(line.clone());
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
            }
            // A terminal change wakes the report at once; a promotion is
            // seen at the next tick.
            let _ = tokio::time::timeout(WATCH_TICK, changes.changed()).await;
        }
    });
    Box::pin(reader)
}

/// The jobs a `jobs.snapshot` answer names, with the runner instance that
/// answered: what a runner's line table carries for each line.
///
/// # Errors
///
/// The value is not a snapshot.
pub fn decode_jobs_snapshot(value: Value) -> Result<(String, Vec<JobView>), ExecutionBackendError> {
    decode_snapshot(value)
}

/// Answers one session verb from a line's `sessions`, as the runner side of
/// a link; `instance` names this runner process in starts and snapshots. A
/// verb of another family is [`ServeFailure::Unsupported`].
///
/// A home-local runner (`home_local`) starts each process in the context
/// its host resolved and sent with the start, and refuses a start without
/// one; any other runner starts processes in its own environment and
/// refuses a start that carries one.
pub fn serve_sessions(
    environment: &ExecutionEnvironment,
    sessions: &JoinedProcessSessions,
    instance: &str,
    verb: &str,
    payload: Value,
    control: OperationContext,
    home_local: bool,
) -> ServeFuture {
    // The job event stream is a stream the runner opens itself.
    let Some(verb) = RunnerVerb::parse(verb).filter(|verb| {
        verb.family() == RunnerVerbFamily::Sessions && *verb != RunnerVerb::JobsWatch
    }) else {
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
                    effect(
                        &environment,
                        &sessions,
                        &instance,
                        verb,
                        payload,
                        control,
                        home_local,
                    )
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
    home_local: bool,
) -> Result<Value, ExecutionBackendError> {
    match verb {
        RunnerVerb::ProcessStart => {
            let (request, next, process) =
                decode_start(wire::open_envelope(environment, payload)?)?;
            let process = match (home_local, process) {
                (true, Some(facts)) => Some(Arc::new(HostProcessContext::from_facts(facts)?)),
                (false, None) => None,
                (true, None) => {
                    return Err(super::super::invalid_request(
                        "a home-local runner starts a process only in its host's process context",
                    ));
                }
                (false, Some(_)) => {
                    return Err(super::super::invalid_request(
                        "a runner starts processes in its own environment",
                    ));
                }
            };
            // The line numbers past every number its host has seen, so a
            // restarted runner never reuses one.
            sessions.seed_job_counter(next);
            let id = sessions.start_in(request, control, process)?;
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
        RunnerVerb::JobsDisown => {
            sessions.disown_leftovers()?;
            Ok(json!({}))
        }
        _ => Err(super::super::backend_error(
            super::super::ExecutionBackendErrorKind::Unsupported,
            "the verb is not a process-session effect",
        )),
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
