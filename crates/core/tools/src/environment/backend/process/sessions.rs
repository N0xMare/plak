//! Joined process sessions and the host-capacity registry.

use crate::environment::ExecutionBackendKind;
use crate::environment::backend::job_output::{JobOutputAdmission, JobOutputWriter, JobOutputs};
use crate::environment::backend::{JOINED_STDIN_CLOSED, JobOutputFile, JobOutputRoot};

use super::{
    Arc, AtomicBool, AtomicU8, AtomicU64, AtomicUsize, BTreeMap, BTreeSet, CancellationToken,
    Condvar, Duration, EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentLimits,
    ExecutionOperationControl, ExecutionWorkspace, HostProcessContext, Instant,
    JOINED_PROCESS_SERVICE_GENERATION, JoinedProcessCommand, JoinedProcessCursorPollResult,
    JoinedProcessInputDisposition, JoinedProcessOutputCursor, JoinedProcessOutputRecord,
    JoinedProcessOutputStream, JoinedProcessPollResult, JoinedProcessSessionId,
    JoinedProcessStartRequest, JoinedProcessStatus, MAX_JOINED_PROCESS_SESSION_POLL_RECORDS,
    MAX_JOINED_PROCESS_SESSION_QUEUE, MAX_JOINED_PROCESS_SESSION_WRITE_BYTES,
    MAX_JOINED_PROCESS_SESSIONS, MAX_LITERAL_PROCESS_STDIN_BYTES, Mutex, NonZeroU64, Ordering,
    PtyInputSender, Receiver, TryLockError, VecDeque, Weak, backend_error,
    cleanup_unresolved_error, close_joined_session_input, conflict_error, fmt, invalid_request,
    io_error, lock, recovery_required_error, resource_error, sleep_until_next_poll,
    start_joined_host_process, sync_channel, write_joined_session_input,
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

pub(super) fn cancel_foreground(ownership: &AtomicU8, control: &ExecutionOperationControl) -> bool {
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
    pub(super) foreground_control: ExecutionOperationControl,
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
                disowned: false,
            })),
            consumer_cursor: Arc::new(Mutex::new(JoinedProcessOutputCursor::new(0))),
            consumer_pending_utf8: Arc::new(Mutex::new(Vec::new())),
            consumer_quiet_polls: Arc::default(),
            consumed: AtomicBool::new(false),
            promoted: Arc::new(AtomicU8::new(PROMOTED)),
            foreground_control: ExecutionOperationControl::new(CancellationToken::new()),
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
    /// Installs a nonblocking host lifecycle observer before any promotion.
    /// Running snapshots reserve terminal capacity; terminal snapshots use it.
    /// `has_room` answers, without recording anything, whether `observer`
    /// would accept a promotion now. Promotion asks it before walking the
    /// jobs root; `observer`'s own answer stays authoritative.
    pub fn observe_jobs(
        &self,
        observer: impl Fn(crate::JobView) -> bool + Send + Sync + 'static,
        has_room: impl Fn() -> bool + Send + Sync + 'static,
    ) {
        *lock(&self.inner.job_observer) = Some(JobObserver {
            record: Arc::new(observer),
            has_room: Arc::new(has_room),
        });
    }

    pub(super) fn observe_job(
        &self,
        id: JoinedProcessSessionId,
        session: &JoinedSession,
        promotion: bool,
    ) -> Result<(), ExecutionBackendError> {
        let observer = lock(&self.inner.job_observer).clone();
        if let Some(observer) = observer {
            let mut view = self.job_view(id, session);
            if promotion {
                view.status = crate::JobStatus::Running;
            }
            if !(observer.record)(view) {
                return Err(resource_error("host job observation queue is full"));
            }
        }
        Ok(())
    }

    pub(crate) fn set_unread_mail_pending(&self, pending: bool) {
        self.inner
            .unread_mail_pending
            .store(pending, Ordering::Release);
    }

    pub(crate) fn unread_mail_pending(&self) -> bool {
        self.inner.unread_mail_pending.load(Ordering::Acquire)
    }

    pub(crate) fn set_notice_delivery_available(&self, available: bool) {
        self.inner
            .notice_delivery_available
            .store(available, Ordering::Release);
    }

    pub(crate) fn notice_delivery_available(&self) -> bool {
        self.inner.notice_delivery_available.load(Ordering::Acquire)
    }

    /// Captures terminal transitions already observed for this service's jobs.
    #[must_use]
    pub fn job_terminal_generation(&self) -> u64 {
        let _ = self.terminal_jobs_since(0);
        u64::try_from(lock(&self.inner.state).terminal_jobs.len()).unwrap_or(u64::MAX)
    }

    /// Subscribes before polling to wake promptly on process terminal changes.
    #[must_use]
    pub fn job_terminal_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.terminal_changes.subscribe()
    }

    pub(crate) fn session_changes(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<tokio::sync::watch::Receiver<u64>, ExecutionBackendError> {
        Ok(lock(&self.session(id)?.shared).changes.subscribe())
    }

    /// Names promoted jobs that became terminal after an admission generation.
    #[must_use]
    pub fn terminal_jobs_since(&self, generation: u64) -> Vec<String> {
        let mut state = lock(&self.inner.state);
        {
            let newly_terminal: Vec<_> = state
                .active
                .iter()
                .chain(&state.finished)
                .filter(|(id, session)| {
                    session.promoted.load(Ordering::Acquire) == PROMOTED
                        && lock(&session.shared).status.is_terminal()
                        && !state.terminal_membership.contains(id)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in &newly_terminal {
                if let Some(session) = state.active.get(id).or_else(|| state.finished.get(id)) {
                    let _ = self.observe_job(*id, session, false);
                }
            }
            state
                .terminal_membership
                .extend(newly_terminal.iter().copied());
            state.terminal_jobs.extend(newly_terminal);
        }
        state
            .terminal_jobs
            .iter()
            .skip(usize::try_from(generation).unwrap_or(usize::MAX))
            .map(|id| format!("p{}", Self::job_number(*id)))
            .collect()
    }

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

    /// Seeds the durable per-session number counter without ever moving it back.
    pub(crate) fn seed_job_counter(&self, next_id: u32) {
        let mut state = lock(&self.inner.state);
        state.next = state.next.max(u64::from(next_id));
    }

    pub(crate) fn restore_job_terminals(&self, views: &[crate::JobView]) {
        let mut state = lock(&self.inner.state);
        for view in views {
            if state
                .active
                .keys()
                .chain(state.finished.keys())
                .any(|id| Self::job_number(*id) == view.number)
            {
                continue;
            }
            // Fresh allocation-local identity authorizes snapshot reads, never a PID.
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&self.inner.owner);
            bytes[8..].copy_from_slice(&u64::from(view.number).to_be_bytes());
            let snapshot = JoinedSession::terminal_snapshot(view, &self.inner);
            state
                .finished
                .insert(JoinedProcessSessionId(bytes), Arc::new(snapshot));
        }
    }

    /// Returns the next number to persist with a checkpoint.
    #[must_use]
    pub(crate) fn next_job_number(&self) -> u32 {
        u32::try_from(lock(&self.inner.state).next).unwrap_or(u32::MAX)
    }

    /// Binds non-secret agent attribution to this process service.
    pub(crate) fn set_job_owner(&self, owner: String) {
        let mut state = lock(&self.inner.state);
        let mut agent = lock(&self.inner.agent);
        *agent = owner;
        state.accepting_jobs = true;
    }

    /// Whether the released owner's process admission gate is closed.
    #[must_use]
    pub fn job_admission_closed(&self) -> bool {
        !lock(&self.inner.state).accepting_jobs
    }

    pub(crate) fn close_job_admission(&self, owner: &str) {
        // A released handle can outlive the new owner of this shared service.
        let mut state = lock(&self.inner.state);
        let agent = lock(&self.inner.agent);
        if *agent == owner {
            state.accepting_jobs = false;
        }
    }

    /// Returns the vendor handle of an allocation-local process identity.
    #[must_use]
    pub fn job_number(id: JoinedProcessSessionId) -> u32 {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&id.0[8..]);
        u32::try_from(u64::from_be_bytes(bytes)).unwrap_or(u32::MAX)
    }

    /// Whether this process is session-owned rather than turn-owned.
    #[must_use]
    pub fn is_promoted(&self, id: JoinedProcessSessionId) -> bool {
        self.session(id)
            .is_ok_and(|session| session.promoted.load(Ordering::Acquire) == PROMOTED)
    }

    /// Leader exit and the observed live-member count for a leftover job.
    /// This allocation-local fact is not authority to recover or signal a PID.
    #[must_use]
    pub fn leftover(&self, id: JoinedProcessSessionId) -> Option<(Option<i32>, usize)> {
        let session = self.session(id).ok()?;
        let shared = lock(&session.shared);
        shared.leftover.map(|job| (job.exit_code, job.processes))
    }

    /// Explicitly gives up ownership of leftover jobs. Their output readers
    /// close, and no later stop, release, or hard cap can reach their groups.
    ///
    /// # Errors
    /// Returns an error if a supervisor cannot finish relinquishing ownership.
    pub fn disown_leftovers(&self) -> Result<(), ExecutionBackendError> {
        let deadline = Instant::now() + self.inner.limits.cleanup_duration();
        let sessions = lock(&self.inner.state)
            .active
            .iter()
            .map(|(id, session)| (*id, Arc::clone(session)))
            .collect::<Vec<_>>();
        let mut pending = Vec::new();
        for (id, session) in sessions {
            let mut shared = lock(&session.shared);
            if session.promoted.load(Ordering::Acquire) != PROMOTED
                || shared.leftover.is_none()
                || shared.status.is_terminal()
            {
                continue;
            }
            shared.disowned = true;
            drop(shared);
            pending.push((id, session));
        }
        for (id, session) in pending {
            let shared = lock(&session.shared);
            let terminal = Arc::clone(&shared.terminal);
            let (shared, _) = terminal
                .wait_timeout_while(
                    shared,
                    deadline.saturating_duration_since(Instant::now()),
                    |shared| !shared.status.is_terminal(),
                )
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !shared.status.is_terminal() {
                return Err(cleanup_unresolved_error());
            }
            let status = shared.status;
            drop(shared);
            let _operation = session.enter_until(deadline)?;
            session.join_finished_owner_until(status, deadline)?;
            self.retire(id, status);
            if status == JoinedProcessStatus::Failed {
                return Err(io_error(
                    "released output handoff failed; the group was stopped",
                ));
            }
        }
        Ok(())
    }

    /// Whether an exact active session or terminal snapshot is still retained.
    #[must_use]
    pub fn retains_session(&self, id: JoinedProcessSessionId) -> bool {
        self.session(id).is_ok()
    }

    /// Promotes a process and detaches dispatch cancellation before producing its handle.
    /// It blocks on the jobs root's retention sweep and budget; async callers
    /// reserve on the blocking pool and promote with [`Self::promote_reserved`].
    ///
    /// # Errors
    /// Rejects a foreign session or an exhausted bounded notice ledger.
    pub(crate) fn promote(&self, id: JoinedProcessSessionId) -> Result<(), ExecutionBackendError> {
        let fits = self.reserve_job_output(id);
        self.promote_reserved(id, fits)
    }

    /// Runs the retention sweep and makes room under the jobs root's budget
    /// for one promoted job's file; returns whether it fits. It walks the
    /// jobs root, so async callers run it on the blocking pool. It publishes
    /// nothing, so a caller dropped after it leaves no job behind. A
    /// promotion the service would refuse now walks nothing and fits nothing.
    pub(crate) fn reserve_job_output(&self, id: JoinedProcessSessionId) -> bool {
        if self.job_output_file(Self::job_number(id)).is_some() {
            return false;
        }
        {
            let Ok(session) = self.session(id) else {
                return false;
            };
            if self
                .promotion_refusal(&lock(&self.inner.state), &session)
                .is_some()
            {
                return false;
            }
        }
        self.prepare_job_output()
    }

    pub(super) fn prepare_job_output(&self) -> bool {
        let Some(scope) = lock(&self.inner.outputs).as_ref().map(JobOutputs::scope) else {
            return false;
        };
        let (swept, evicted, fits) = scope.prepare_create(std::time::SystemTime::now());
        if let Some(outputs) = lock(&self.inner.outputs).as_mut() {
            outputs.retire(&swept, &evicted);
        }
        fits
    }

    /// The result promotion returns without publishing: `Ok` when already
    /// promoted, else a refusal while the owner releases, after foreground
    /// cancellation won, at the unconsumed-row limit, or while the host's
    /// notice ledger has no room.
    pub(super) fn promotion_refusal(
        &self,
        state: &JoinedProcessServiceState,
        session: &JoinedSession,
    ) -> Option<Result<(), ExecutionBackendError>> {
        if session.promoted.load(Ordering::Acquire) == PROMOTED {
            return Some(Ok(()));
        }
        if !state.accepting_jobs {
            return Some(Err(backend_error(
                ExecutionBackendErrorKind::Cancelled,
                "process owner is releasing; job promotion is closed",
            )));
        }
        if cancel_foreground(&session.promoted, &session.foreground_control) {
            return Some(Err(backend_error(
                ExecutionBackendErrorKind::Cancelled,
                "foreground cancellation won before job promotion",
            )));
        }
        let rows = state
            .active
            .values()
            .chain(state.finished.values())
            .filter(|session| session.promoted.load(Ordering::Acquire) == PROMOTED)
            .count();
        if rows >= 128 {
            return Some(Err(
                resource_error("unconsumed job row limit was reached").with_proven_no_effect(false)
            ));
        }
        let observer = lock(&self.inner.job_observer).clone();
        observer
            .is_some_and(|observer| !(observer.has_room)())
            .then(|| {
                Err(resource_error("host job observation queue is full")
                    .with_proven_no_effect(false))
            })
    }

    /// Promotes after [`Self::reserve_job_output`]: publication, the job's
    /// file (when it fit) and its monitor, with no root walk, so async callers
    /// run it inline and nothing can drop them between publication and their
    /// result. The monitor starts before publication, so a failure publishes
    /// nothing.
    ///
    /// # Errors
    /// Rejects a foreign session, an exhausted bounded notice ledger or a
    /// monitor that could not start.
    pub(crate) fn promote_reserved(
        &self,
        id: JoinedProcessSessionId,
        fits: bool,
    ) -> Result<(), ExecutionBackendError> {
        let session = self.session(id)?;
        // The monitor waits for `published`; dropping it unsent ends the
        // monitor without touching the foreground session.
        let (published, gate) = sync_channel::<()>(1);
        {
            let state = lock(&self.inner.state);
            if let Some(refusal) = self.promotion_refusal(&state, &session) {
                return refusal;
            }
            self.spawn_job_monitor(id, &session, gate)?;
            if session
                .promoted
                .compare_exchange(FOREGROUND, PROMOTED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(backend_error(
                    ExecutionBackendErrorKind::Cancelled,
                    "foreground cancellation won before job promotion",
                ));
            }
            if let Err(error) = self.observe_job(id, &session, true) {
                session.promoted.store(FOREGROUND, Ordering::Release);
                return Err(error.with_proven_no_effect(false));
            }
        }
        if fits {
            self.spool_promoted(id, &session);
        }
        let _ = published.send(());
        Ok(())
    }

    /// Starts a promoted job's monitor, which acts only once `gate` reports
    /// publication: it joins the job once terminal and then closes its spool.
    pub(super) fn spawn_job_monitor(
        &self,
        id: JoinedProcessSessionId,
        session: &JoinedSession,
        gate: Receiver<()>,
    ) -> Result<(), ExecutionBackendError> {
        let weak = Arc::downgrade(&self.inner);
        let shared = Arc::clone(&session.shared);
        let terminal = Arc::clone(&lock(&shared).terminal);
        // Waiting owns only the status buffer, not the service or process
        // supervisor. A private owner drop still stops its process groups.
        std::thread::Builder::new()
            .name(String::from("plak-job-monitor"))
            .spawn(move || {
                if gate.recv().is_err() {
                    return;
                }
                let status = terminal
                    .wait_while(lock(&shared), |state| {
                        state.status == JoinedProcessStatus::Running && !state.released
                    })
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let joined = !status.released && status.status.is_terminal();
                drop(status);
                // Unresolved cleanup has no more supervisor events; retain
                // its owner for recovery rather than leaking a waiter.
                if joined && let Some(inner) = weak.upgrade() {
                    let _ = Self { inner }.join(id);
                }
                // Closing the spool ends the file; consumption marks it later.
                drop(lock(&shared).spool.take());
            })
            .map(drop)
            .map_err(|_| resource_error("job monitor could not start").with_proven_no_effect(false))
    }

    /// Joins exactly one exited process without signalling another process.
    ///
    /// # Errors
    /// Rejects running/foreign sessions or a failed supervisor join.
    pub fn join(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        let session = self.session(id)?;
        let _operation = session.enter()?;
        let status = lock(&session.shared).status;
        if !status.is_terminal() {
            return Err(conflict_error("process is still running"));
        }
        session.join_finished_owner(status)?;
        self.retire(id, status);
        Ok(status)
    }

    /// Stops one process with recorded attribution.
    ///
    /// # Errors
    /// Rejects foreign sessions or unresolved cleanup.
    pub fn stop(
        &self,
        id: JoinedProcessSessionId,
        by: crate::JobStopReason,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        self.stop_with_grace(id, by, Duration::from_secs(2))
    }

    /// Stops one process with a bounded TERM grace (200 ms for confirmed quit).
    ///
    /// # Errors
    /// Rejects foreign sessions, invalid grace, or unresolved cleanup.
    pub fn stop_with_grace(
        &self,
        id: JoinedProcessSessionId,
        by: crate::JobStopReason,
        grace: Duration,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        if grace > Duration::from_secs(2) {
            return Err(invalid_request("process stop grace exceeds two seconds"));
        }
        {
            let session = self.session(id)?;
            if !lock(&session.shared).status.is_terminal() {
                lock(&session.stopped_by).get_or_insert(by);
                session.stop_grace_millis.store(
                    u64::try_from(grace.as_millis()).unwrap_or(2000),
                    Ordering::Release,
                );
            }
        }
        self.terminate(id)
    }

    /// Stops promoted jobs concurrently under the service cleanup bound.
    ///
    /// # Errors
    /// Reports any unresolved process-group join after attempting every job.
    pub fn stop_jobs(
        &self,
        by: crate::JobStopReason,
        grace: Duration,
    ) -> Result<(), ExecutionBackendError> {
        let ids = self
            .active_session_ids()
            .into_iter()
            .filter(|id| self.is_promoted(*id))
            .collect();
        self.stop_job_ids(ids, by, grace)
    }

    pub(crate) fn stop_jobs_for_owner(
        &self,
        owner: &str,
        by: crate::JobStopReason,
        grace: Duration,
    ) -> Result<(), ExecutionBackendError> {
        let ids = {
            let state = lock(&self.inner.state);
            if *lock(&self.inner.agent) != owner {
                return Ok(());
            }
            {
                state
                    .active
                    .iter()
                    .filter(|(_, session)| session.promoted.load(Ordering::Acquire) == PROMOTED)
                    .map(|(id, _)| *id)
                    .collect()
            }
        };
        self.stop_job_ids(ids, by, grace)
    }

    pub(super) fn stop_job_ids(
        &self,
        ids: Vec<JoinedProcessSessionId>,
        by: crate::JobStopReason,
        grace: Duration,
    ) -> Result<(), ExecutionBackendError> {
        let workers = ids
            .into_iter()
            .map(|id| {
                let sessions = self.clone();
                std::thread::spawn(move || sessions.stop_with_grace(id, by, grace))
            })
            .collect::<Vec<_>>();
        let mut failure = None;
        for worker in workers {
            match worker.join() {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    failure.get_or_insert(error);
                }
                Err(_) => {
                    failure.get_or_insert_with(cleanup_unresolved_error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Stops only foreground sessions before a host transfers this service.
    ///
    /// # Errors
    /// Reports the first unresolved cleanup after attempting all foreground ids.
    pub fn stop_foreground(&self) -> Result<(), ExecutionBackendError> {
        let mut failure = None;
        for id in self
            .active_session_ids()
            .into_iter()
            .filter(|id| !self.is_promoted(*id))
        {
            if let Err(error) = self.terminate(id) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Drops snapshots only after their terminal observation has committed.
    pub(crate) fn consume_jobs(&self, numbers: &[u32]) {
        let _ = self.terminal_jobs_since(0);
        {
            let mut released = Vec::new();
            let mut state = lock(&self.inner.state);
            for (id, session) in &state.active {
                if numbers.contains(&Self::job_number(*id))
                    && lock(&session.shared).status.is_terminal()
                {
                    // Retirement removes the row only after the owner has joined.
                    session.consumed.store(true, Ordering::Release);
                    released.push(Self::job_number(*id));
                }
            }
            let consumed = state
                .finished
                .keys()
                .copied()
                .filter(|id| numbers.contains(&Self::job_number(*id)))
                .collect::<Vec<_>>();
            for id in consumed {
                if let Some(session) = state.finished.remove(&id) {
                    released.push(Self::job_number(id));
                    state
                        .tombstones
                        .push_back((id, lock(&session.shared).status));
                }
            }
            while state.tombstones.len() > self.inner.active_limit {
                state.tombstones.pop_front();
            }
            drop(state);
            self.release_job_outputs(released);
        }
    }

    /// Binds the host's private jobs root: from now on every promotion spools
    /// its combined output to an exclusive file and issues its path.
    pub fn bind_job_outputs(
        &self,
        root: JobOutputRoot,
        session: plakous_model::SessionId,
        branch: plakous_model::BranchId,
    ) {
        *lock(&self.inner.outputs) = Some(JobOutputs::new(root, session, branch, self.inner.owner));
    }

    /// The output file issued for a promoted job number, while it exists.
    #[must_use]
    pub fn job_output_file(&self, number: u32) -> Option<JobOutputFile> {
        lock(&self.inner.outputs)
            .as_ref()
            .and_then(|outputs| outputs.file(number))
    }

    /// Writes one finished tool result's complete text to a fresh issued
    /// output file and returns it already consumed: readable through the read
    /// exception like a promoted job's file, then retained and swept like a
    /// consumed one. The file takes the next job number, so no later job
    /// reuses its path. `None` when no jobs root is bound, its budget has no
    /// room, or the file could not be created. It walks the jobs root, so
    /// async callers run it on the blocking pool.
    #[must_use]
    pub fn spill_output(&self, bytes: &[u8]) -> Option<JobOutputFile> {
        if !self.prepare_job_output() {
            return None;
        }
        let number = {
            let mut state = lock(&self.inner.state);
            let number = u32::try_from(state.next).ok()?;
            state.next = state
                .next
                .checked_add(1)
                .filter(|next| u32::try_from(*next).is_ok())?;
            number
        };
        let file = {
            let mut outputs = lock(&self.inner.outputs);
            let outputs = outputs.as_mut()?;
            let mut writer = outputs.create(number).ok()?;
            writer.append(bytes);
            drop(writer);
            outputs.file(number)?
        };
        self.release_job_outputs(vec![number]);
        Some(file)
    }

    /// Decides the read exception for `path`; `None` leaves it to containment.
    pub(in crate::environment::backend) fn admit_job_output(
        &self,
        path: &str,
    ) -> Option<JobOutputAdmission> {
        lock(&self.inner.outputs)
            .as_ref()
            .and_then(|outputs| outputs.admit(path))
    }

    /// Marks consumed jobs' files and runs the retention sweep on the blocking
    /// pool when a runtime is present (inline otherwise), never under the
    /// outputs lock; the lock is retaken only to forget deleted files.
    pub(super) fn release_job_outputs(&self, numbers: Vec<u32>) {
        if numbers.is_empty() {
            return;
        }
        let Some((scope, own)) = lock(&self.inner.outputs)
            .as_ref()
            .map(|outputs| (outputs.scope(), outputs.issued(&numbers)))
        else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let release = move || {
            let swept = scope.release(&numbers, &own, std::time::SystemTime::now());
            if !swept.is_empty()
                && let Some(inner) = weak.upgrade()
                && let Some(outputs) = lock(&inner.outputs).as_mut()
            {
                outputs.retire(&swept, &[]);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(release)),
            Err(_) => release(),
        }
    }

    /// Creates the promoted job's file and moves spooling under the output
    /// lock, so no record is lost or written twice. The caller reserved room
    /// first. Failure never blocks promotion; the job then has no issued path.
    pub(super) fn spool_promoted(&self, id: JoinedProcessSessionId, session: &JoinedSession) {
        let mut shared = lock(&session.shared);
        self.spool_output(id, &mut shared);
    }

    pub(super) fn spool_output(
        &self,
        id: JoinedProcessSessionId,
        shared: &mut JoinedSessionShared,
    ) {
        let mut outputs = lock(&self.inner.outputs);
        let Some(outputs) = outputs.as_mut() else {
            return;
        };
        if outputs.file(Self::job_number(id)).is_some() {
            return;
        }
        let Ok(mut writer) = outputs.create(Self::job_number(id)) else {
            return;
        };
        let evicted = shared
            .output
            .records
            .front()
            .map_or(shared.output.next_cursor, |record| record.cursor);
        if evicted > 0 {
            writer.append(
                format!("[... {evicted} bytes of output omitted before promotion ...]\n")
                    .as_bytes(),
            );
        }
        for record in &shared.output.records {
            writer.append(&record.bytes);
        }
        // The job monitor closes the file once the job is terminal.
        shared.spool = Some(writer);
        shared.pending_spool = None;
    }

    /// Whether the service retains any promoted running or unconsumed job.
    #[must_use]
    pub fn has_jobs(&self) -> bool {
        let state = lock(&self.inner.state);
        state
            .active
            .values()
            .chain(state.finished.values())
            .any(|session| session.promoted.load(Ordering::Acquire) == PROMOTED)
    }

    /// Counts promoted running jobs without copying retained output.
    #[must_use]
    pub fn running_job_count(&self) -> usize {
        lock(&self.inner.state)
            .active
            .values()
            .filter(|session| {
                session.promoted.load(Ordering::Acquire) == PROMOTED
                    && !lock(&session.shared).status.is_terminal()
            })
            .count()
    }

    /// Whether a promoted job ended and its terminal notice is still
    /// unconsumed. A job whose notice was consumed stays listed by
    /// [`Self::jobs`] with its terminal status until its owner joins it, so a
    /// terminal status alone does not mean unreported.
    #[must_use]
    pub fn has_unreported_terminal_job(&self) -> bool {
        let state = lock(&self.inner.state);
        state
            .active
            .values()
            .chain(state.finished.values())
            .any(|session| {
                session.promoted.load(Ordering::Acquire) == PROMOTED
                    && !session.consumed.load(Ordering::Acquire)
                    && lock(&session.shared).status.is_terminal()
            })
    }

    /// Returns bounded live and pending-notice snapshots.
    #[must_use]
    pub fn jobs(&self) -> Vec<crate::JobView> {
        let state = lock(&self.inner.state);
        state
            .active
            .iter()
            .chain(&state.finished)
            .filter(|(_, session)| session.promoted.load(Ordering::Acquire) == PROMOTED)
            .map(|(id, session)| self.job_view(*id, session))
            .collect()
    }

    pub(super) fn job_view(
        &self,
        id: JoinedProcessSessionId,
        session: &JoinedSession,
    ) -> crate::JobView {
        let shared = lock(&session.shared);
        let status = match shared.status {
            JoinedProcessStatus::Running => crate::JobStatus::Running,
            JoinedProcessStatus::Exited(code) => crate::JobStatus::Exited(code),
            JoinedProcessStatus::TimedOut => {
                crate::JobStatus::Stopped(crate::JobStopReason::HardCap)
            }
            JoinedProcessStatus::Terminated => lock(&session.stopped_by)
                .map_or(crate::JobStatus::OutcomeUnknown, crate::JobStatus::Stopped),
            _ => crate::JobStatus::OutcomeUnknown,
        };
        let mut bytes = shared
            .output
            .records
            .iter()
            .rev()
            .flat_map(|record| record.bytes.iter().rev().copied())
            .take(4096)
            .collect::<Vec<_>>();
        bytes.reverse();
        let tail = crate::internal::job_output_tail(&String::from_utf8_lossy(&bytes));
        crate::JobView {
            number: Self::job_number(id),
            command: session.command.clone(),
            started_at: session.started_at,
            owner: lock(&self.inner.agent).clone(),
            elapsed: shared.elapsed.unwrap_or_else(|| session.started.elapsed()),
            status,
            output: tail,
            session_id: id.0,
            owner_generation: self.inner.owner_generation.get(),
        }
    }

    /// Resolves a vendor handle only within this service's live/snapshot namespace.
    #[must_use]
    pub fn session_for_number(&self, number: u32) -> Option<JoinedProcessSessionId> {
        let state = lock(&self.inner.state);
        state
            .active
            .keys()
            .chain(state.finished.keys())
            .find(|id| Self::job_number(**id) == number)
            .copied()
    }

    /// Resolves an already-reported job while its bounded terminal tombstone remains.
    #[must_use]
    pub(crate) fn reported_session_for_number(
        &self,
        number: u32,
    ) -> Option<(JoinedProcessSessionId, JoinedProcessStatus)> {
        lock(&self.inner.state)
            .tombstones
            .iter()
            .find(|(id, _)| Self::job_number(*id) == number)
            .copied()
    }

    pub(crate) fn projection_state(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<crate::internal::ProcessSessionProjectionState, ExecutionBackendError> {
        let session = self.session(id)?;
        Ok((
            Arc::clone(&session.consumer_cursor),
            Arc::clone(&session.consumer_pending_utf8),
            Arc::clone(&session.consumer_quiet_polls),
        ))
    }

    pub(crate) fn runtime_owner_generation(&self) -> NonZeroU64 {
        self.inner.owner_generation
    }

    pub(super) fn new(environment: &ExecutionEnvironment) -> Result<Self, ExecutionBackendError> {
        if environment.backend().kind() != ExecutionBackendKind::Host {
            return Err(backend_error(
                ExecutionBackendErrorKind::Unsupported,
                "joined process sessions require the direct host backend",
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

    /// Starts one pipes- or Unix-PTY-mode process and returns a non-PID opaque identity.
    ///
    /// # Errors
    ///
    /// Rejects unsupported modes, exhausted bounds, invalid paths, spawn/setup
    /// failures, or an already-cancelled operation.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "start consumes one exact request at its process-dispatch boundary"
    )]
    pub fn start(
        &self,
        request: JoinedProcessStartRequest,
        control: ExecutionOperationControl,
    ) -> Result<JoinedProcessSessionId, ExecutionBackendError> {
        control.check_cancelled()?;
        let control = control.with_preempt(tokio::sync::watch::channel(crate::ToolPreempt::None).1);
        let _host_reservation = lock(&self.inner.host_capacity)
            .as_ref()
            .map(JoinedProcessHostCapacity::reserve)
            .transpose()?;
        // A child that has exited holds no concurrency. Reclaim finished
        // sessions before admitting a new one, so a sequential program that
        // starts one command at a time never exhausts the limit.
        self.reclaim_finished_sessions();
        let id = {
            let mut state = lock(&self.inner.state);
            if !state.accepting_jobs {
                return Err(backend_error(
                    ExecutionBackendErrorKind::Cancelled,
                    "process owner is releasing; process admission is closed",
                ));
            }
            if Self::running_sessions(&state).saturating_add(state.starting)
                >= self.inner.active_limit
            {
                let running = state
                    .active
                    .iter()
                    .filter(|(_, session)| {
                        lock(&session.shared).status == JoinedProcessStatus::Running
                    })
                    .map(|(id, _)| format!("p{}", Self::job_number(*id)))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ExecutionBackendError::new(
                    ExecutionBackendErrorKind::ResourceLimit,
                    format!(
                        "running process limit ({}) was reached: {running}",
                        self.inner.active_limit
                    ),
                ));
            }
            let sequence = state.next;
            state.next = state
                .next
                .checked_add(1)
                .filter(|next| u32::try_from(*next).is_ok())
                .ok_or_else(|| resource_error("joined process session identity space exhausted"))?;
            state.starting = state.starting.saturating_add(1);
            let mut bytes = [0_u8; 16];
            bytes[..8].copy_from_slice(&self.inner.owner);
            bytes[8..].copy_from_slice(&sequence.to_be_bytes());
            JoinedProcessSessionId(bytes)
        };
        let started = start_joined_host_process(
            &self.inner.workspace,
            &self.inner.process,
            &self.inner.environment,
            self.inner.limits,
            self.inner.queue_limit,
            &request,
            control,
            Some(PendingJobOutput::new(
                &self.inner,
                id,
                request.output_spool_threshold,
            )),
        );
        let mut state = lock(&self.inner.state);
        state.starting = state.starting.saturating_sub(1);
        match started {
            Ok(session) => {
                let mut shared = lock(&session.shared);
                shared.terminal_changes = Some(self.inner.terminal_changes.clone());
                let terminal = shared.status.is_terminal();
                drop(shared);
                state.active.insert(id, Arc::new(session));
                if terminal {
                    self.inner
                        .terminal_changes
                        .send_modify(|revision| *revision = revision.wrapping_add(1));
                }
                Ok(id)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn start_with_output_limit(
        &self,
        mut request: JoinedProcessStartRequest,
        control: ExecutionOperationControl,
        max_output_bytes: usize,
    ) -> Result<JoinedProcessSessionId, ExecutionBackendError> {
        request.output_spool_threshold = Some(max_output_bytes);
        self.start(request, control)
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

    /// Writes bounded bytes to the exact open stdin and optionally closes it.
    ///
    /// # Errors
    ///
    /// Rejects foreign/stale sessions, closed stdin, queue/input bounds, and
    /// bounded I/O failures.
    pub fn write_stdin(
        &self,
        id: JoinedProcessSessionId,
        bytes: &[u8],
        disposition: JoinedProcessInputDisposition,
    ) -> Result<(), ExecutionBackendError> {
        if bytes.len() > MAX_JOINED_PROCESS_SESSION_WRITE_BYTES {
            return Err(resource_error(
                "joined process stdin write exceeds its bound",
            ));
        }
        let session = self.session(id)?;
        let _operation = session.enter()?;
        if lock(&session.shared).status.is_terminal() {
            return Err(conflict_error("joined process session is already terminal"));
        }
        let prior = session.input_bytes.fetch_add(bytes.len(), Ordering::AcqRel);
        if prior.saturating_add(bytes.len()) > MAX_LITERAL_PROCESS_STDIN_BYTES {
            session.input_bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
            return Err(resource_error(
                "joined process aggregate input exceeds its bound",
            ));
        }
        let mut input = lock(&session.input);
        let writer = input
            .as_mut()
            .ok_or_else(|| conflict_error(JOINED_STDIN_CLOSED))?;
        write_joined_session_input(writer, bytes, session.input_timeout)?;
        if disposition == JoinedProcessInputDisposition::Close {
            close_joined_session_input(writer, session.input_timeout)?;
            input.take();
        }
        Ok(())
    }

    /// Returns bounded newly available records and the current status.
    ///
    /// # Errors
    ///
    /// Rejects invalid poll bounds, foreign/stale sessions, queue saturation,
    /// or a failed supervisor join.
    pub fn poll(
        &self,
        id: JoinedProcessSessionId,
        max_bytes: usize,
        max_records: usize,
    ) -> Result<JoinedProcessPollResult, ExecutionBackendError> {
        if max_bytes == 0
            || u64::try_from(max_bytes).unwrap_or(u64::MAX)
                > self.inner.limits.max_combined_output_bytes()
            || max_records == 0
            || max_records > MAX_JOINED_PROCESS_SESSION_POLL_RECORDS
        {
            return Err(invalid_request("joined process poll bounds are invalid"));
        }
        let session = self.session(id)?;
        let _operation = session.enter()?;
        let mut shared = lock(&session.shared);
        let status = shared.status;
        let mut remaining = max_bytes;
        let mut records = Vec::new();
        while records.len() < max_records {
            let Some(front) = shared.output.records.front() else {
                break;
            };
            if front.bytes.len() > remaining {
                let returned = JoinedProcessOutputRecord {
                    sequence: front.sequence,
                    cursor: front.cursor,
                    stream: front.stream,
                    bytes: Arc::from(&front.bytes[..remaining]),
                };
                let retained = JoinedProcessOutputRecord {
                    sequence: front.sequence,
                    cursor: front
                        .cursor
                        .saturating_add(u64::try_from(remaining).unwrap_or(u64::MAX)),
                    stream: front.stream,
                    bytes: Arc::from(&front.bytes[remaining..]),
                };
                shared.output.records.pop_front();
                shared.output.records.push_front(retained);
                shared.output.retained_bytes = shared
                    .output
                    .retained_bytes
                    .saturating_sub(u64::try_from(remaining).unwrap_or(u64::MAX));
                records.push(returned);
                break;
            }
            let Some(record) = shared.output.records.pop_front() else {
                break;
            };
            remaining -= record.bytes.len();
            shared.output.retained_bytes = shared
                .output
                .retained_bytes
                .saturating_sub(u64::try_from(record.bytes.len()).unwrap_or(u64::MAX));
            records.push(record);
        }
        let more_output = !shared.output.records.is_empty();
        drop(shared);
        session.join_finished_owner(status)?;
        Ok(JoinedProcessPollResult {
            records: records.into(),
            status,
            more_output,
        })
    }

    /// Reads bounded output from an exact cursor without consuming retained bytes.
    ///
    /// A cursor behind the retained window (its bytes were evicted to keep the
    /// window bounded) reads from the window's start and reports the skipped
    /// byte count.
    ///
    /// # Errors
    ///
    /// Rejects foreign/stale sessions, invalid bounds, or a cursor ahead of the
    /// output produced so far.
    pub fn poll_from(
        &self,
        id: JoinedProcessSessionId,
        cursor: JoinedProcessOutputCursor,
        max_bytes: usize,
        max_records: usize,
    ) -> Result<JoinedProcessCursorPollResult, ExecutionBackendError> {
        if max_bytes == 0
            || u64::try_from(max_bytes).unwrap_or(u64::MAX)
                > self.inner.limits.max_combined_output_bytes()
            || max_records == 0
            || max_records > MAX_JOINED_PROCESS_SESSION_POLL_RECORDS
        {
            return Err(invalid_request("joined process poll bounds are invalid"));
        }
        let session = self.session(id)?;
        let _operation = session.enter()?;
        let shared = lock(&session.shared);
        let oldest = shared
            .output
            .records
            .front()
            .map_or(shared.output.next_cursor, |record| record.cursor);
        if cursor.0 > shared.output.next_cursor {
            return Err(conflict_error(
                "joined process output cursor is stale or unknown",
            ));
        }
        let skipped_bytes = oldest.saturating_sub(cursor.0);
        let cursor = JoinedProcessOutputCursor(cursor.0.max(oldest));
        let mut remaining = max_bytes;
        let mut records = Vec::new();
        let mut next = cursor.0;
        for record in &shared.output.records {
            if records.len() >= max_records || remaining == 0 {
                break;
            }
            let end = record
                .cursor
                .saturating_add(u64::try_from(record.bytes.len()).unwrap_or(u64::MAX));
            if end <= cursor.0 {
                continue;
            }
            let offset = usize::try_from(cursor.0.saturating_sub(record.cursor))
                .unwrap_or(usize::MAX)
                .min(record.bytes.len());
            let count = remaining.min(record.bytes.len().saturating_sub(offset));
            records.push(JoinedProcessOutputRecord {
                sequence: record.sequence,
                cursor: record
                    .cursor
                    .saturating_add(u64::try_from(offset).unwrap_or(u64::MAX)),
                stream: record.stream,
                bytes: Arc::from(&record.bytes[offset..offset.saturating_add(count)]),
            });
            remaining -= count;
            next = next.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        }
        let more_output = next < shared.output.next_cursor;
        let status = shared.status;
        drop(shared);
        session.join_finished_owner(status)?;
        Ok(JoinedProcessCursorPollResult {
            records: records.into(),
            status,
            next_cursor: JoinedProcessOutputCursor(next),
            more_output,
            skipped_bytes,
        })
    }

    /// Reads the newest bounded tail without walking or copying the whole window.
    ///
    /// # Errors
    /// Rejects invalid bounds, foreign/stale sessions, or queue saturation.
    pub(crate) fn poll_tail(
        &self,
        id: JoinedProcessSessionId,
        max_bytes: usize,
        max_records: usize,
    ) -> Result<JoinedProcessCursorPollResult, ExecutionBackendError> {
        if max_bytes == 0
            || u64::try_from(max_bytes).unwrap_or(u64::MAX)
                > self.inner.limits.max_combined_output_bytes()
            || max_records == 0
            || max_records > MAX_JOINED_PROCESS_SESSION_POLL_RECORDS
        {
            return Err(invalid_request("joined process poll bounds are invalid"));
        }
        let session = self.session(id)?;
        let _operation = session.enter()?;
        let shared = lock(&session.shared);
        let mut remaining = max_bytes;
        let mut records = Vec::new();
        for record in shared.output.records.iter().rev().take(max_records) {
            if remaining == 0 {
                break;
            }
            let count = remaining.min(record.bytes.len());
            let offset = record.bytes.len() - count;
            records.push(JoinedProcessOutputRecord {
                sequence: record.sequence,
                cursor: record
                    .cursor
                    .saturating_add(u64::try_from(offset).unwrap_or(u64::MAX)),
                stream: record.stream,
                bytes: Arc::from(&record.bytes[offset..]),
            });
            remaining -= count;
        }
        records.reverse();
        let skipped_bytes = records
            .first()
            .map_or(shared.output.next_cursor, |record| record.cursor);
        Ok(JoinedProcessCursorPollResult {
            records: records.into(),
            status: shared.status,
            next_cursor: JoinedProcessOutputCursor(shared.output.next_cursor),
            more_output: false,
            skipped_bytes,
        })
    }

    /// Terminates and joins one exact session, then retains a bounded tombstone.
    ///
    /// # Errors
    ///
    /// Rejects foreign/stale sessions or reports a failed supervisor join.
    pub fn terminate(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        let deadline = Instant::now() + self.inner.limits.cleanup_duration();
        let session = self.session(id)?;
        let _operation = session.enter_until(deadline)?;
        if !lock(&session.shared).status.is_terminal() {
            lock(&session.stopped_by).get_or_insert(crate::JobStopReason::Model);
        }
        lock(&session.input).take();
        let mut owner = lock(&session.owner);
        let joined = if let Some(owner) = owner.as_mut() {
            owner.stop.store(true, Ordering::Release);
            owner.join_until(deadline)?
        } else {
            true
        };
        if !joined {
            return Err(cleanup_unresolved_error());
        }
        owner.take();
        drop(owner);
        let status = lock(&session.shared).status;
        if status == JoinedProcessStatus::CleanupUnresolved {
            return Err(recovery_required_error(
                "joined process cleanup remains unresolved",
            ));
        }
        self.retire(id, status);
        Ok(status)
    }

    /// Terminates and joins every active session owned by this service.
    ///
    /// The joins run concurrently under one cleanup deadline. Walking the
    /// sessions serially charges every supervisor's scheduling delay against
    /// that same window, so a full running set returns `CleanupUnresolved`
    /// under load even though each group would have joined on its own. Stop is
    /// still requested only after that session's operation lock is held, so a
    /// busy session stays active for a retry.
    ///
    /// # Errors
    ///
    /// Returns the first join failure after attempting every active session.
    /// A worker panic is resumed after the others have been joined; it is not
    /// a retryable `CleanupUnresolved`.
    pub fn join_all(&self) -> Result<(), ExecutionBackendError> {
        let ids = lock(&self.inner.state)
            .active
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let deadline = Instant::now() + self.inner.limits.cleanup_duration();
        // `retire` releases job outputs on the caller's runtime. These workers
        // are fresh threads, so enter that runtime and the sweep matches the
        // serial path instead of always running inline.
        let runtime = tokio::runtime::Handle::try_current().ok();
        let mut workers = Vec::with_capacity(ids.len());
        let mut failure = None;
        for id in ids {
            let sessions = self.clone();
            let runtime = runtime.clone();
            match std::thread::Builder::new()
                .name("plak-join".to_owned())
                .spawn(move || {
                    let _entered = runtime.as_ref().map(|handle| handle.enter());
                    sessions.terminate_until(id, deadline)
                }) {
                Ok(worker) => workers.push(worker),
                Err(_) => {
                    if let Err(error) = self.terminate_until(id, deadline) {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        let mut panicked = None;
        for worker in workers {
            match worker.join() {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    failure.get_or_insert(error);
                }
                Err(payload) => {
                    panicked.get_or_insert(payload);
                }
            }
        }
        if let Some(payload) = panicked {
            std::panic::resume_unwind(payload);
        }
        failure.map_or(Ok(()), Err)
    }

    pub(super) fn terminate_until(
        &self,
        id: JoinedProcessSessionId,
        deadline: Instant,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        let session = self.session(id)?;
        let _operation = session.enter_until(deadline)?;
        if !lock(&session.shared).status.is_terminal() {
            lock(&session.stopped_by).get_or_insert(crate::JobStopReason::SessionExit);
        }
        lock(&session.input).take();
        let mut owner = lock(&session.owner);
        if let Some(owner) = owner.as_mut() {
            owner.stop.store(true, Ordering::Release);
            if !owner.join_until(deadline)? {
                return Err(cleanup_unresolved_error());
            }
        }
        owner.take();
        drop(owner);
        let status = lock(&session.shared).status;
        if status == JoinedProcessStatus::CleanupUnresolved {
            return Err(recovery_required_error(
                "joined process cleanup remains unresolved",
            ));
        }
        self.retire(id, status);
        Ok(status)
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
