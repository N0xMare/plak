//! Joined process sessions and the host-capacity registry.

mod io;
mod job_output;
mod jobs;
mod stop;

use crate::environment::backend::job_output::{JobOutputAdmission, JobOutputWriter, JobOutputs};
use crate::environment::backend::{JOINED_STDIN_CLOSED, JobOutputFile, JobOutputRoot};
use crate::environment::{ExecutionBackendKind, RunnerId};

use super::{
    Arc, AtomicBool, AtomicU8, AtomicU64, AtomicUsize, BTreeMap, BTreeSet, CancellationToken,
    Condvar, Duration, EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentLimits,
    ExecutionWorkspace, HostProcessContext, Instant, JOINED_PROCESS_SERVICE_GENERATION,
    JoinedProcessCommand, JoinedProcessCursorPollResult, JoinedProcessInputDisposition,
    JoinedProcessOutputCursor, JoinedProcessOutputRecord, JoinedProcessOutputStream,
    JoinedProcessPollResult, JoinedProcessSessionId, JoinedProcessStartRequest,
    JoinedProcessStatus, MAX_JOINED_PROCESS_SESSION_POLL_RECORDS, MAX_JOINED_PROCESS_SESSION_QUEUE,
    MAX_JOINED_PROCESS_SESSION_WRITE_BYTES, MAX_JOINED_PROCESS_SESSIONS,
    MAX_LITERAL_PROCESS_STDIN_BYTES, Mutex, NonZeroU64, OperationContext, Ordering, PtyInputSender,
    Receiver, TryLockError, VecDeque, Weak, backend_error, cleanup_unresolved_error,
    close_joined_session_input, conflict_error, fmt, invalid_request, io_error, lock,
    recovery_required_error, resource_error, sleep_until_next_poll, start_joined_host_process,
    sync_channel, write_joined_session_input,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HostProcessStop {
    Completed,
    Cancelled,
    TimedOut,
    OutputLimit,
    Io,
}

#[derive(Default)]
pub(super) struct HostProcessOutput {
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(super) enum HostProcessStream {
    Stdout,
    Stderr,
}

pub(super) struct HostProcessOwner {
    pub(super) stop: Arc<AtomicBool>,
    pub(super) thread: Option<std::thread::JoinHandle<()>>,
}

impl HostProcessOwner {
    pub(super) fn join_until(&mut self, deadline: Instant) -> Result<bool, ExecutionBackendError> {
        let Some(thread) = self.thread.as_ref() else {
            // An owner is removed after a successful join. A retained owner
            // without its handle means that a prior join observed a panic.
            return Err(io_error("host process supervisor failed"));
        };
        let started = Instant::now();
        while Instant::now() < deadline && !thread.is_finished() {
            sleep_until_next_poll(started, deadline);
        }
        if !thread.is_finished() {
            return Ok(false);
        }
        let thread = self
            .thread
            .take()
            .ok_or_else(|| io_error("host process supervisor failed"))?;
        thread
            .join()
            .map_err(|_| io_error("host process supervisor failed"))?;
        Ok(true)
    }
}

impl Drop for HostProcessOwner {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.stop.store(true, Ordering::Release);
        drop(thread);
    }
}

#[derive(Default)]
pub(super) struct JoinedSessionOutput {
    pub(super) records: VecDeque<JoinedProcessOutputRecord>,
    pub(super) retained_bytes: u64,
    pub(super) total_stdout_bytes: u64,
    pub(super) total_stderr_bytes: u64,
    pub(super) next_sequence: u64,
    pub(super) next_cursor: u64,
}

pub(super) struct JoinedSessionShared {
    pub(super) output: JoinedSessionOutput,
    pub(super) status: JoinedProcessStatus,
    pub(super) elapsed: Option<Duration>,
    pub(super) terminal_changes: Option<tokio::sync::watch::Sender<u64>>,
    pub(super) changes: tokio::sync::watch::Sender<u64>,
    pub(super) terminal: Arc<Condvar>,
    pub(super) released: bool,
    pub(super) spool: Option<JobOutputWriter>,
    pub(super) pending_spool: Option<PendingJobOutput>,
    pub(super) leftover: Option<LeftoverJob>,
    pub(super) disowned: bool,
}

#[derive(Clone, Copy)]
pub(super) struct LeftoverJob {
    pub(super) exit_code: Option<i32>,
    pub(super) processes: usize,
}

pub(super) struct PendingJobOutput {
    pub(super) service: Weak<JoinedProcessServiceInner>,
    pub(super) id: JoinedProcessSessionId,
    pub(super) threshold: u64,
    pub(super) display_bytes: u64,
    pub(super) utf8: Vec<u8>,
}

impl PendingJobOutput {
    pub(super) fn new(
        service: &Arc<JoinedProcessServiceInner>,
        id: JoinedProcessSessionId,
        limit: Option<usize>,
    ) -> Self {
        let window = service.limits.max_combined_output_bytes();
        Self {
            service: Arc::downgrade(service),
            id,
            threshold: limit.map_or(window, |limit| {
                u64::try_from(limit).unwrap_or(u64::MAX).min(window)
            }),
            display_bytes: 0,
            utf8: Vec::new(),
        }
    }

    pub(super) fn observe(&mut self, bytes: &[u8], terminal: bool) -> bool {
        self.utf8.extend_from_slice(bytes);
        let mut consumed = 0;
        while consumed < self.utf8.len() {
            match std::str::from_utf8(&self.utf8[consumed..]) {
                Ok(text) => {
                    self.display_bytes = self.display_bytes.saturating_add(text.len() as u64);
                    consumed = self.utf8.len();
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    self.display_bytes = self.display_bytes.saturating_add(valid as u64);
                    consumed += valid;
                    if let Some(invalid) = error.error_len() {
                        self.display_bytes = self.display_bytes.saturating_add(3);
                        consumed += invalid;
                    } else {
                        if terminal {
                            self.display_bytes = self.display_bytes.saturating_add(3);
                            consumed = self.utf8.len();
                        }
                        break;
                    }
                }
            }
        }
        self.utf8.drain(..consumed);
        self.display_bytes > self.threshold
    }
}

impl JoinedSessionShared {
    pub(super) fn terminal_changed(&mut self) {
        self.maybe_spool(&[], true);
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        self.terminal.notify_all();
        if let Some(changes) = &self.terminal_changes {
            changes.send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    pub(super) fn maybe_spool(&mut self, bytes: &[u8], terminal: bool) {
        let needed = self.pending_spool.as_mut().is_some_and(|pending| {
            pending.observe(bytes, terminal) || self.output.next_cursor > pending.threshold
        });
        if needed
            && let Some(pending) = self.pending_spool.take()
            && let Some(inner) = pending.service.upgrade()
        {
            // Capture the whole prefix before eviction, including raw bytes
            // whose lossy UTF-8 presentation exceeds the page's byte bound.
            let sessions = JoinedProcessSessions { inner };
            if sessions.prepare_job_output() {
                sessions.spool_output(pending.id, self);
            }
        }
    }
}

pub(super) enum JoinedSessionInput {
    Pipes(std::process::ChildStdin),
    Pty(PtyInputSender),
}

// Promotion and dispatch cancellation compete for the same foreground owner.
pub(super) const FOREGROUND: u8 = 0;

const PROMOTED: u8 = 1;

const CANCELLING: u8 = 2;

pub(super) fn cancel_foreground(ownership: &AtomicU8, control: &OperationContext) -> bool {
    if control.cancellation().is_cancelled() {
        let _ =
            ownership.compare_exchange(FOREGROUND, CANCELLING, Ordering::AcqRel, Ordering::Acquire);
    }
    ownership.load(Ordering::Acquire) == CANCELLING
}

pub(super) struct JoinedSession {
    pub(super) operation: Mutex<()>,
    pub(super) queued: AtomicUsize,
    pub(super) queue_limit: usize,
    pub(super) input: Mutex<Option<JoinedSessionInput>>,
    pub(super) input_bytes: AtomicUsize,
    pub(super) input_timeout: Duration,
    pub(super) cleanup_duration: Duration,
    pub(super) shared: Arc<Mutex<JoinedSessionShared>>,
    pub(super) consumer_cursor: Arc<Mutex<JoinedProcessOutputCursor>>,
    pub(super) consumer_pending_utf8: Arc<Mutex<Vec<u8>>>,
    pub(super) consumer_quiet_polls: Arc<Mutex<crate::internal::ProcessQuietPollState>>,
    pub(super) promoted: Arc<AtomicU8>,
    pub(super) consumed: AtomicBool,
    pub(super) foreground_control: OperationContext,
    pub(super) stop_grace_millis: Arc<AtomicU64>,
    pub(super) command: String,
    pub(super) started_at: plakous_model::Timestamp,
    pub(super) started: Instant,
    pub(super) ended: Mutex<Option<Duration>>,
    pub(super) stopped_by: Mutex<Option<crate::JobStopReason>>,
    pub(super) owner: Mutex<Option<HostProcessOwner>>,
}

impl Drop for JoinedSession {
    fn drop(&mut self) {
        let mut shared = lock(&self.shared);
        shared.released = true;
        shared.terminal.notify_all();
    }
}

impl JoinedSession {
    pub(super) fn terminal_snapshot(
        view: &crate::JobView,
        service: &JoinedProcessServiceInner,
    ) -> Self {
        let (status, stopper) = match view.status {
            crate::JobStatus::Exited(code) => (JoinedProcessStatus::Exited(code), None),
            crate::JobStatus::Stopped(crate::JobStopReason::HardCap) => (
                JoinedProcessStatus::TimedOut,
                Some(crate::JobStopReason::HardCap),
            ),
            crate::JobStatus::Stopped(by) => (JoinedProcessStatus::Terminated, Some(by)),
            crate::JobStatus::Released => (JoinedProcessStatus::Released, None),
            crate::JobStatus::Running | crate::JobStatus::OutcomeUnknown => {
                (JoinedProcessStatus::OutcomeUnknown, None)
            }
        };
        let output_bytes: Arc<[u8]> = Arc::from(view.output.as_bytes());
        let size = u64::try_from(output_bytes.len()).unwrap_or(u64::MAX);
        let output = JoinedSessionOutput {
            records: VecDeque::from([JoinedProcessOutputRecord {
                sequence: 0,
                cursor: 0,
                stream: JoinedProcessOutputStream::Stdout,
                bytes: output_bytes,
            }]),
            retained_bytes: size,
            total_stdout_bytes: size,
            total_stderr_bytes: 0,
            next_sequence: 1,
            next_cursor: size,
        };
        Self {
            operation: Mutex::new(()),
            queued: AtomicUsize::new(0),
            queue_limit: service.queue_limit,
            input: Mutex::new(None),
            input_bytes: AtomicUsize::new(0),
            input_timeout: service.limits.execution_duration(),
            cleanup_duration: service.limits.cleanup_duration(),
            shared: Arc::new(Mutex::new(JoinedSessionShared {
                output,
                status,
                elapsed: Some(view.elapsed),
                terminal_changes: None,
                changes: tokio::sync::watch::channel(0).0,
                terminal: Arc::new(Condvar::new()),
                released: false,
                spool: None,
                pending_spool: None,
                leftover: None,
                disowned: view.status == crate::JobStatus::Released,
            })),
            consumer_cursor: Arc::new(Mutex::new(JoinedProcessOutputCursor::new(0))),
            consumer_pending_utf8: Arc::new(Mutex::new(Vec::new())),
            consumer_quiet_polls: Arc::default(),
            consumed: AtomicBool::new(false),
            promoted: Arc::new(AtomicU8::new(PROMOTED)),
            foreground_control: OperationContext::new(CancellationToken::new()),
            stop_grace_millis: Arc::new(AtomicU64::new(2000)),
            command: view.command.clone(),
            started_at: view.started_at,
            started: Instant::now(),
            ended: Mutex::new(Some(view.elapsed)),
            stopped_by: Mutex::new(stopper),
            owner: Mutex::new(None),
        }
    }

    pub(super) fn enter(&self) -> Result<JoinedSessionOperation<'_>, ExecutionBackendError> {
        let previous = self.queued.fetch_add(1, Ordering::AcqRel);
        if previous >= self.queue_limit {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(resource_error(
                "joined process session operation queue is full",
            ));
        }
        let guard = lock(&self.operation);
        Ok(JoinedSessionOperation {
            session: self,
            _guard: guard,
        })
    }

    pub(super) fn enter_until(
        &self,
        deadline: Instant,
    ) -> Result<JoinedSessionOperation<'_>, ExecutionBackendError> {
        let previous = self.queued.fetch_add(1, Ordering::AcqRel);
        if previous >= self.queue_limit {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(resource_error(
                "joined process session operation queue is full",
            ));
        }
        loop {
            match self.operation.try_lock() {
                Ok(guard) => {
                    return Ok(JoinedSessionOperation {
                        session: self,
                        _guard: guard,
                    });
                }
                Err(TryLockError::Poisoned(poisoned)) => {
                    return Ok(JoinedSessionOperation {
                        session: self,
                        _guard: poisoned.into_inner(),
                    });
                }
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TryLockError::WouldBlock) => {
                    self.queued.fetch_sub(1, Ordering::AcqRel);
                    return Err(cleanup_unresolved_error());
                }
            }
        }
    }

    pub(super) fn join_finished_owner(
        &self,
        status: JoinedProcessStatus,
    ) -> Result<(), ExecutionBackendError> {
        self.join_finished_owner_until(status, Instant::now() + self.cleanup_duration)
    }

    pub(super) fn join_finished_owner_until(
        &self,
        status: JoinedProcessStatus,
        deadline: Instant,
    ) -> Result<(), ExecutionBackendError> {
        if status == JoinedProcessStatus::CleanupUnresolved {
            return Err(recovery_required_error(
                "joined process cleanup remains unresolved",
            ));
        }
        if status.is_terminal() {
            let mut owner = lock(&self.owner);
            if let Some(owner) = owner.as_mut()
                && !owner.join_until(deadline)?
            {
                return Err(cleanup_unresolved_error());
            }
            owner.take();
        }
        Ok(())
    }
}

pub(super) struct JoinedSessionOperation<'a> {
    pub(super) session: &'a JoinedSession,
    pub(super) _guard: std::sync::MutexGuard<'a, ()>,
}

impl Drop for JoinedSessionOperation<'_> {
    fn drop(&mut self) {
        self.session.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct JoinedProcessServiceState {
    pub(super) next: u64,
    pub(super) terminal_jobs: Vec<JoinedProcessSessionId>,
    pub(super) terminal_membership: BTreeSet<JoinedProcessSessionId>,
    pub(super) accepting_jobs: bool,
    pub(super) starting: usize,
    pub(super) active: BTreeMap<JoinedProcessSessionId, Arc<JoinedSession>>,
    pub(super) finished: BTreeMap<JoinedProcessSessionId, Arc<JoinedSession>>,
    pub(super) tombstones: VecDeque<(JoinedProcessSessionId, JoinedProcessStatus)>,
}

/// The host's job lifecycle observer and its cheap promotion room check.
#[derive(Clone)]
pub(super) struct JobObserver {
    pub(super) record: Arc<dyn Fn(crate::JobView) -> bool + Send + Sync>,
    pub(super) has_room: Arc<dyn Fn() -> bool + Send + Sync>,
}

pub(super) struct JoinedProcessServiceInner {
    pub(super) owner: [u8; 8],
    pub(super) owner_generation: NonZeroU64,
    pub(super) workspace: ExecutionWorkspace,
    pub(super) process: Arc<HostProcessContext>,
    pub(super) environment: Arc<[(EnvironmentVariableName, EnvironmentVariableValue)]>,
    pub(super) limits: ExecutionEnvironmentLimits,
    pub(super) active_limit: usize,
    pub(super) queue_limit: usize,
    pub(super) state: Mutex<JoinedProcessServiceState>,
    pub(super) terminal_changes: tokio::sync::watch::Sender<u64>,
    pub(super) notice_delivery_available: AtomicBool,
    pub(super) unread_mail_pending: AtomicBool,
    pub(super) agent: Mutex<String>,
    pub(super) host_capacity: Mutex<Option<JoinedProcessHostCapacity>>,
    pub(super) job_observer: Mutex<Option<JobObserver>>,
    pub(super) outputs: Mutex<Option<JobOutputs>>,
}

#[derive(Default)]
pub(in crate::environment) struct JoinedProcessServiceCache {
    pub(super) service: Mutex<Option<Arc<JoinedProcessServiceInner>>>,
}

/// Process-local host service owning joined child/group/pipe lifetimes.
///
/// Clone shares the same allocation-local owner generation. Session IDs are
/// neither serializable nor reconstructible. Call [`Self::join_all`] before a
/// normal owner shutdown; `Drop` is emergency cleanup authority only.
#[derive(Clone)]
pub struct JoinedProcessSessions {
    pub(super) inner: Arc<JoinedProcessServiceInner>,
}

/// Shared host-wide admission bound for process services (128 running starts).
#[derive(Clone, Default)]
pub struct JoinedProcessHostCapacity {
    pub(super) state: Arc<Mutex<JoinedProcessHostCapacityState>>,
}

#[derive(Default)]
pub(super) struct JoinedProcessHostCapacityState {
    pub(super) starting: usize,
    pub(super) services: Vec<Weak<JoinedProcessServiceInner>>,
}

impl fmt::Debug for JoinedProcessHostCapacity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinedProcessHostCapacity")
            .finish_non_exhaustive()
    }
}

pub(super) struct HostStartReservation(JoinedProcessHostCapacity);

impl Drop for HostStartReservation {
    fn drop(&mut self) {
        lock(&self.0.state).starting -= 1;
    }
}

impl JoinedProcessHostCapacity {
    pub(super) fn reserve(&self) -> Result<HostStartReservation, ExecutionBackendError> {
        let mut state = lock(&self.state);
        state.services.retain(|service| service.strong_count() != 0);
        let running = state
            .services
            .iter()
            .filter_map(Weak::upgrade)
            .map(|service| JoinedProcessSessions::running_sessions(&lock(&service.state)))
            .sum::<usize>();
        if running.saturating_add(state.starting) >= 128 {
            return Err(resource_error(
                "host running process limit (128) was reached",
            ));
        }
        state.starting += 1;
        Ok(HostStartReservation(self.clone()))
    }
}

impl fmt::Debug for JoinedProcessSessions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.inner.state);
        formatter
            .debug_struct("JoinedProcessSessions")
            .field("active", &state.active.len())
            .field("active_limit", &self.inner.active_limit)
            .field("queue_limit", &self.inner.queue_limit)
            .finish()
    }
}

impl JoinedProcessSessions {
    /// Enrolls this service in a host registry's shared pre-spawn capacity bound.
    pub fn bind_host_capacity(&self, capacity: JoinedProcessHostCapacity) {
        let mut state = lock(&capacity.state);
        let weak = Arc::downgrade(&self.inner);
        if !state.services.iter().any(|service| service.ptr_eq(&weak)) {
            state.services.push(weak);
        }
        drop(state);
        *lock(&self.inner.host_capacity) = Some(capacity);
    }

    pub(super) fn new(environment: &ExecutionEnvironment) -> Result<Self, ExecutionBackendError> {
        if environment.backend().kind() != ExecutionBackendKind::Runner
            || !environment
                .backend()
                .runner_id()
                .is_some_and(RunnerId::is_local)
        {
            return Err(backend_error(
                ExecutionBackendErrorKind::Unsupported,
                "joined process sessions require the local runner's direct backend",
            ));
        }
        environment.verify_operation_backend()?;
        let resolved = environment
            .config()
            .environment_delegations()
            .iter()
            .filter_map(|binding| {
                environment
                    .operation_backend
                    .resolve_environment(binding)
                    .map(|value| (binding.name().clone(), value))
            })
            .collect::<Vec<_>>();
        let authority = environment.backend().authority_id().bytes();
        let generation = JOINED_PROCESS_SERVICE_GENERATION
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| resource_error("joined process service generation space exhausted"))?;
        let owner_generation = NonZeroU64::new(generation)
            .ok_or_else(|| resource_error("joined process owner generation became zero"))?;
        let mut authority_prefix = [0_u8; 8];
        authority_prefix.copy_from_slice(&authority[..8]);
        let owner = (u64::from_be_bytes(authority_prefix) ^ generation).to_be_bytes();
        Ok(Self {
            inner: Arc::new(JoinedProcessServiceInner {
                owner,
                owner_generation,
                workspace: ExecutionWorkspace::from_environment(environment),
                process: environment.host_process.clone().ok_or_else(|| {
                    invalid_request("host process construction material is unavailable")
                })?,
                environment: resolved.into(),
                limits: environment.config().limits(),
                active_limit: MAX_JOINED_PROCESS_SESSIONS,
                queue_limit: MAX_JOINED_PROCESS_SESSION_QUEUE,
                agent: Mutex::new(String::from("private")),
                host_capacity: Mutex::new(None),
                job_observer: Mutex::new(None),
                outputs: Mutex::new(None),
                terminal_changes: tokio::sync::watch::channel(0).0,
                notice_delivery_available: AtomicBool::new(true),
                unread_mail_pending: AtomicBool::new(false),
                state: Mutex::new(JoinedProcessServiceState {
                    next: 1,
                    terminal_jobs: Vec::new(),
                    terminal_membership: BTreeSet::new(),
                    accepting_jobs: true,
                    starting: 0,
                    active: BTreeMap::new(),
                    finished: BTreeMap::new(),
                    tombstones: VecDeque::new(),
                }),
            }),
        })
    }

    /// The joined process sessions this environment still holds.
    #[must_use]
    pub fn active_session_ids(&self) -> Vec<JoinedProcessSessionId> {
        lock(&self.inner.state).active.keys().copied().collect()
    }

    /// Returns one session's joined terminal status, when it has one.
    ///
    /// A terminal status means this service has no live input authority, so
    /// a refused write cannot have an effect. Exact terminal evidence proves
    /// group cleanup; a read-only `OutcomeUnknown` snapshot does not claim
    /// anything about the former owner's process. `None`
    /// covers a running session, a session whose cleanup is unresolved, and an
    /// identity this service never issued or no longer remembers: none of
    /// those prove anything about an operation's effect, and each one stays on
    /// the unknown-outcome path.
    #[must_use]
    pub fn joined_terminal_status(
        &self,
        id: JoinedProcessSessionId,
    ) -> Option<JoinedProcessStatus> {
        if id.0[..8] != self.inner.owner {
            return None;
        }
        let state = lock(&self.inner.state);
        if let Some(session) = state.active.get(&id).or_else(|| state.finished.get(&id)) {
            let status = lock(&session.shared).status;
            return status.is_terminal().then_some(status);
        }
        state
            .tombstones
            .iter()
            .find(|(retired, _)| *retired == id)
            .map(|(_, status)| *status)
    }

    /// Identities with backend proof that the child group has exited and
    /// joined. Includes bounded tombstones for finished sessions reclaimed to
    /// admit later sequential commands.
    pub(crate) fn joined_terminal_identities(&self) -> Vec<[u8; 16]> {
        let state = lock(&self.inner.state);
        let active = state
            .active
            .iter()
            .map(|(id, session)| (*id, Arc::clone(session)))
            .collect::<Vec<_>>();
        let mut joined = state
            .tombstones
            .iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        drop(state);

        // A supervisor publishes terminal status before its thread exits.
        // Only its joined owner (or a retired tombstone) proves cleanup.
        // Release the service lock before taking an operation lock: retire
        // takes those locks in the opposite order.
        for (id, session) in active {
            let Ok(_operation) = session.enter_until(Instant::now()) else {
                continue;
            };
            let status = lock(&session.shared).status;
            if status.is_terminal()
                && session
                    .join_finished_owner_until(status, Instant::now())
                    .is_ok()
            {
                joined.push(id.0);
            }
        }
        joined
    }

    /// Reports whether no started or starting session remains.
    #[must_use]
    pub fn is_drained(&self) -> bool {
        let state = lock(&self.inner.state);
        state.active.is_empty() && state.starting == 0
    }

    #[cfg(test)]
    pub(super) fn cleanup_duration(&self) -> Duration {
        self.inner.limits.cleanup_duration()
    }

    /// Counts the sessions whose group is still running.
    ///
    /// The active limit bounds concurrent children, not remembered ones: a
    /// session that has exited and been joined occupies no process slot and is
    /// kept only so its final output can still be read.
    pub(super) fn running_sessions(state: &JoinedProcessServiceState) -> usize {
        state
            .active
            .values()
            .filter(|session| !lock(&session.shared).status.is_terminal())
            .count()
    }

    /// Releases finished sessions once too many are being remembered.
    ///
    /// A finished session is kept so its final output can still be drained and
    /// its exit status still read, which is what makes polling a finished
    /// session work. Keeping them is bounded rather than free: at most
    /// `active_limit` finished sessions are retained, and older ones are
    /// released oldest-first. Session identities are allocated in sequence, so
    /// the active map is already in creation order.
    pub(super) fn reclaim_finished_sessions(&self) {
        let finished = {
            let state = lock(&self.inner.state);
            state
                .active
                .iter()
                .filter_map(|(id, session)| {
                    lock(&session.shared).status.is_terminal().then_some(*id)
                })
                .collect::<Vec<_>>()
        };
        // Everything but the newest `active_limit`.
        let release = finished.len().saturating_sub(self.inner.active_limit);
        for id in finished.into_iter().take(release) {
            // Best effort: a session another operation holds stays for the next
            // admission, and the limit check still applies either way.
            drop(self.terminate(id));
        }
    }

    pub(super) fn session(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<Arc<JoinedSession>, ExecutionBackendError> {
        if id.0[..8] != self.inner.owner {
            return Err(backend_error(
                ExecutionBackendErrorKind::IdentityMismatch,
                "joined process session belongs to a foreign owner generation",
            ));
        }
        let state = lock(&self.inner.state);
        if let Some(session) = state.active.get(&id).or_else(|| state.finished.get(&id)) {
            return Ok(Arc::clone(session));
        }
        if state.tombstones.iter().any(|(retired, _)| *retired == id) {
            return Err(conflict_error(
                "joined process session is stale and already joined",
            ));
        }
        Err(invalid_request(
            "joined process session identity is unknown",
        ))
    }

    pub(super) fn retire(&self, id: JoinedProcessSessionId, status: JoinedProcessStatus) {
        let mut state = lock(&self.inner.state);
        if state.finished.contains_key(&id)
            || state.tombstones.iter().any(|(retired, _)| *retired == id)
        {
            return;
        }
        if state.active.get(&id).is_some_and(|session| {
            session.promoted.load(Ordering::Acquire) == PROMOTED && status.is_terminal()
        }) && !state.terminal_membership.contains(&id)
        {
            if let Some(session) = state.active.get(&id) {
                let _ = self.observe_job(id, session, false);
            }
            state.terminal_membership.insert(id);
            state.terminal_jobs.push(id);
        }
        if let Some(session) = state.active.remove(&id)
            && session.promoted.load(Ordering::Acquire) == PROMOTED
            && !session.consumed.load(Ordering::Acquire)
        {
            *lock(&session.ended) = Some(session.started.elapsed());
            state.finished.insert(id, session);
            return;
        }
        state.tombstones.push_back((id, status));
        while state.tombstones.len() > self.inner.active_limit {
            state.tombstones.pop_front();
        }
        drop(state);
        if self.job_output_file(Self::job_number(id)).is_some() {
            self.release_job_outputs(vec![Self::job_number(id)]);
        }
    }
}

pub(super) fn displayed_command(command: &JoinedProcessCommand) -> String {
    let text = match command {
        JoinedProcessCommand::Shell { command, .. } => command.to_string(),
        JoinedProcessCommand::Literal {
            executable,
            arguments,
        } => format!("{} {}", executable.display(), arguments.join(" ")),
    };
    crate::internal::sanitize_job_command(&text)
}

impl ExecutionEnvironment {
    /// Installs a host-transferred process service for this live environment.
    pub fn adopt_joined_process_sessions(&self, sessions: &JoinedProcessSessions) {
        *lock(&self.joined_process_sessions.service) = Some(Arc::clone(&sessions.inner));
    }

    /// Returns the already-bound service without allocating a new owner.
    #[must_use]
    pub fn existing_joined_process_sessions(&self) -> Option<JoinedProcessSessions> {
        lock(&self.joined_process_sessions.service)
            .clone()
            .map(|inner| JoinedProcessSessions { inner })
    }

    /// Creates one process-local joined-session service bound to this exact
    /// live host environment. The service is intentionally non-durable.
    ///
    /// # Errors
    ///
    /// Rejects non-host or mismatched live backend authority.
    pub fn joined_process_sessions(&self) -> Result<JoinedProcessSessions, ExecutionBackendError> {
        let mut cached = lock(&self.joined_process_sessions.service);
        if let Some(inner) = cached.clone() {
            return Ok(JoinedProcessSessions { inner });
        }
        let created = JoinedProcessSessions::new(self)?;
        *cached = Some(Arc::clone(&created.inner));
        Ok(created)
    }
}
