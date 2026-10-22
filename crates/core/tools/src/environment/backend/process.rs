//! Owned literal and joined process resources, supervision and cleanup.

use plakous_model::CancellationToken;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::fs::OFlags;
use rustix::io::Errno;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Condvar;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use std::time::Instant;
use std::{fmt, fs};

use std::os::unix::{fs::MetadataExt, process::CommandExt};

#[cfg(test)]
use super::ExecutionEnvironmentConfig;
use super::{
    EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionBackendFuture, ExecutionEnvironment,
    ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits,
    ExecutionOperationControl, ExecutionWorkspace, JOINED_PROCESS_PTY_INITIAL_COLUMNS,
    JOINED_PROCESS_PTY_INITIAL_ROWS, JOINED_PROCESS_SERVICE_GENERATION, JoinedProcessCommand,
    JoinedProcessCursorPollResult, JoinedProcessInputDisposition, JoinedProcessOutputCursor,
    JoinedProcessOutputRecord, JoinedProcessOutputStream, JoinedProcessPollResult,
    JoinedProcessSessionId, JoinedProcessStartRequest, JoinedProcessStatus,
    JoinedProcessTerminalMode, LiteralProcessResult, MAX_EXECUTION_VALUE_BYTES,
    MAX_JOINED_PROCESS_SESSION_POLL_RECORDS, MAX_JOINED_PROCESS_SESSION_QUEUE,
    MAX_JOINED_PROCESS_SESSION_WRITE_BYTES, MAX_JOINED_PROCESS_SESSIONS,
    MAX_LITERAL_PROCESS_ARGUMENTS, MAX_LITERAL_PROCESS_STDIN_BYTES, PreparedLiteralProcess,
    backend_error, cleanup_unresolved_error, conflict_error, invalid_request, io_error, lock,
    recovery_required_error, resolve_existing_no_symlink, resource_error, workspace_error,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostProcessStop {
    Completed,
    Cancelled,
    TimedOut,
    OutputLimit,
    Io,
}

#[derive(Default)]
struct HostProcessOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Clone, Copy)]
enum HostProcessStream {
    Stdout,
    Stderr,
}

/// How long a supervisor sleeps between checks of a live child. A young child
/// is checked every 2 ms so short commands keep their exit latency; a child
/// past one second is checked every 32 ms, so a long-lived job wakes about 30
/// times a second rather than 500 for its whole life.
fn supervision_poll_interval(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_millis(100) {
        Duration::from_millis(2)
    } else if elapsed < Duration::from_secs(1) {
        Duration::from_millis(8)
    } else {
        Duration::from_millis(32)
    }
}

/// One backoff step of a wait that has lasted `elapsed`, cut short so that it
/// never sleeps past a deadline `remaining` away.
fn supervision_sleep(elapsed: Duration, remaining: Duration) -> Duration {
    supervision_poll_interval(elapsed).min(remaining)
}

fn sleep_until_next_poll(started: Instant, deadline: Instant) {
    std::thread::sleep(supervision_sleep(
        started.elapsed(),
        deadline.saturating_duration_since(Instant::now()),
    ));
}

struct HostProcessOwner {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HostProcessOwner {
    fn join_until(&mut self, deadline: Instant) -> Result<bool, ExecutionBackendError> {
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
struct JoinedSessionOutput {
    records: VecDeque<JoinedProcessOutputRecord>,
    retained_bytes: u64,
    total_stdout_bytes: u64,
    total_stderr_bytes: u64,
    next_sequence: u64,
    next_cursor: u64,
}

struct JoinedSessionShared {
    output: JoinedSessionOutput,
    status: JoinedProcessStatus,
    elapsed: Option<Duration>,
    terminal_changes: Option<tokio::sync::watch::Sender<u64>>,
    changes: tokio::sync::watch::Sender<u64>,
    terminal: Arc<Condvar>,
    released: bool,
    spool: Option<super::job_output::JobOutputWriter>,
    pending_spool: Option<PendingJobOutput>,
    leftover: Option<LeftoverJob>,
    disowned: bool,
}

#[derive(Clone, Copy)]
struct LeftoverJob {
    exit_code: Option<i32>,
    processes: usize,
}

struct PendingJobOutput {
    service: Weak<JoinedProcessServiceInner>,
    id: JoinedProcessSessionId,
    threshold: u64,
    display_bytes: u64,
    utf8: Vec<u8>,
}

impl PendingJobOutput {
    fn new(
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

    fn observe(&mut self, bytes: &[u8], terminal: bool) -> bool {
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
    fn terminal_changed(&mut self) {
        self.maybe_spool(&[], true);
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        self.terminal.notify_all();
        if let Some(changes) = &self.terminal_changes {
            changes.send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    fn maybe_spool(&mut self, bytes: &[u8], terminal: bool) {
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

enum JoinedSessionInput {
    Pipes(std::process::ChildStdin),
    Pty(PtyInputSender),
}

// Promotion and dispatch cancellation compete for the same foreground owner.
const FOREGROUND: u8 = 0;
const PROMOTED: u8 = 1;
const CANCELLING: u8 = 2;

fn cancel_foreground(ownership: &AtomicU8, control: &ExecutionOperationControl) -> bool {
    if control.cancellation().is_cancelled() {
        let _ =
            ownership.compare_exchange(FOREGROUND, CANCELLING, Ordering::AcqRel, Ordering::Acquire);
    }
    ownership.load(Ordering::Acquire) == CANCELLING
}

struct JoinedSession {
    operation: Mutex<()>,
    queued: AtomicUsize,
    queue_limit: usize,
    input: Mutex<Option<JoinedSessionInput>>,
    input_bytes: AtomicUsize,
    input_timeout: Duration,
    cleanup_duration: Duration,
    shared: Arc<Mutex<JoinedSessionShared>>,
    consumer_cursor: Arc<Mutex<JoinedProcessOutputCursor>>,
    consumer_pending_utf8: Arc<Mutex<Vec<u8>>>,
    consumer_quiet_polls: Arc<Mutex<crate::internal::ProcessQuietPollState>>,
    promoted: Arc<AtomicU8>,
    consumed: AtomicBool,
    foreground_control: ExecutionOperationControl,
    stop_grace_millis: Arc<AtomicU64>,
    command: String,
    started_at: plakous_model::Timestamp,
    started: Instant,
    ended: Mutex<Option<Duration>>,
    stopped_by: Mutex<Option<crate::JobStopReason>>,
    owner: Mutex<Option<HostProcessOwner>>,
}

impl Drop for JoinedSession {
    fn drop(&mut self) {
        let mut shared = lock(&self.shared);
        shared.released = true;
        shared.terminal.notify_all();
    }
}

impl JoinedSession {
    fn terminal_snapshot(view: &crate::JobView, service: &JoinedProcessServiceInner) -> Self {
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

    fn enter(&self) -> Result<JoinedSessionOperation<'_>, ExecutionBackendError> {
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

    fn enter_until(
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

    fn join_finished_owner(
        &self,
        status: JoinedProcessStatus,
    ) -> Result<(), ExecutionBackendError> {
        self.join_finished_owner_until(status, Instant::now() + self.cleanup_duration)
    }

    fn join_finished_owner_until(
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

struct JoinedSessionOperation<'a> {
    session: &'a JoinedSession,
    _guard: std::sync::MutexGuard<'a, ()>,
}

impl Drop for JoinedSessionOperation<'_> {
    fn drop(&mut self) {
        self.session.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

struct JoinedProcessServiceState {
    next: u64,
    terminal_jobs: Vec<JoinedProcessSessionId>,
    terminal_membership: BTreeSet<JoinedProcessSessionId>,
    accepting_jobs: bool,
    starting: usize,
    active: BTreeMap<JoinedProcessSessionId, Arc<JoinedSession>>,
    finished: BTreeMap<JoinedProcessSessionId, Arc<JoinedSession>>,
    tombstones: VecDeque<(JoinedProcessSessionId, JoinedProcessStatus)>,
}

/// The host's job lifecycle observer and its cheap promotion room check.
#[derive(Clone)]
struct JobObserver {
    record: Arc<dyn Fn(crate::JobView) -> bool + Send + Sync>,
    has_room: Arc<dyn Fn() -> bool + Send + Sync>,
}

struct JoinedProcessServiceInner {
    owner: [u8; 8],
    owner_generation: NonZeroU64,
    workspace: ExecutionWorkspace,
    process: Arc<HostProcessContext>,
    environment: Arc<[(EnvironmentVariableName, EnvironmentVariableValue)]>,
    limits: ExecutionEnvironmentLimits,
    active_limit: usize,
    queue_limit: usize,
    state: Mutex<JoinedProcessServiceState>,
    terminal_changes: tokio::sync::watch::Sender<u64>,
    notice_delivery_available: AtomicBool,
    unread_mail_pending: AtomicBool,
    agent: Mutex<String>,
    host_capacity: Mutex<Option<JoinedProcessHostCapacity>>,
    job_observer: Mutex<Option<JobObserver>>,
    outputs: Mutex<Option<super::job_output::JobOutputs>>,
}

#[derive(Default)]
pub(in crate::environment) struct JoinedProcessServiceCache {
    service: Mutex<Option<Arc<JoinedProcessServiceInner>>>,
}

/// Process-local host service owning joined child/group/pipe lifetimes.
///
/// Clone shares the same allocation-local owner generation. Session IDs are
/// neither serializable nor reconstructible. Call [`Self::join_all`] before a
/// normal owner shutdown; `Drop` is emergency cleanup authority only.
#[derive(Clone)]
pub struct JoinedProcessSessions {
    inner: Arc<JoinedProcessServiceInner>,
}

/// Shared host-wide admission bound for process services (128 running starts).
#[derive(Clone, Default)]
pub struct JoinedProcessHostCapacity {
    state: Arc<Mutex<JoinedProcessHostCapacityState>>,
}

#[derive(Default)]
struct JoinedProcessHostCapacityState {
    starting: usize,
    services: Vec<Weak<JoinedProcessServiceInner>>,
}

impl fmt::Debug for JoinedProcessHostCapacity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinedProcessHostCapacity")
            .finish_non_exhaustive()
    }
}

struct HostStartReservation(JoinedProcessHostCapacity);

impl Drop for HostStartReservation {
    fn drop(&mut self) {
        lock(&self.0.state).starting -= 1;
    }
}

impl JoinedProcessHostCapacity {
    fn reserve(&self) -> Result<HostStartReservation, ExecutionBackendError> {
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

    fn observe_job(
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

    fn prepare_job_output(&self) -> bool {
        let Some(scope) = lock(&self.inner.outputs)
            .as_ref()
            .map(super::job_output::JobOutputs::scope)
        else {
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
    fn promotion_refusal(
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
    fn spawn_job_monitor(
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

    fn stop_job_ids(
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
        root: super::JobOutputRoot,
        session: plakous_model::SessionId,
        branch: plakous_model::BranchId,
    ) {
        *lock(&self.inner.outputs) = Some(super::job_output::JobOutputs::new(
            root,
            session,
            branch,
            self.inner.owner,
        ));
    }

    /// The output file issued for a promoted job number, while it exists.
    #[must_use]
    pub fn job_output_file(&self, number: u32) -> Option<super::JobOutputFile> {
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
    pub fn spill_output(&self, bytes: &[u8]) -> Option<super::JobOutputFile> {
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
    pub(super) fn admit_job_output(
        &self,
        path: &str,
    ) -> Option<super::job_output::JobOutputAdmission> {
        lock(&self.inner.outputs)
            .as_ref()
            .and_then(|outputs| outputs.admit(path))
    }

    /// Marks consumed jobs' files and runs the retention sweep on the blocking
    /// pool when a runtime is present (inline otherwise), never under the
    /// outputs lock; the lock is retaken only to forget deleted files.
    fn release_job_outputs(&self, numbers: Vec<u32>) {
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
    fn spool_promoted(&self, id: JoinedProcessSessionId, session: &JoinedSession) {
        let mut shared = lock(&session.shared);
        self.spool_output(id, &mut shared);
    }

    fn spool_output(&self, id: JoinedProcessSessionId, shared: &mut JoinedSessionShared) {
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

    fn job_view(&self, id: JoinedProcessSessionId, session: &JoinedSession) -> crate::JobView {
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

    fn new(environment: &ExecutionEnvironment) -> Result<Self, ExecutionBackendError> {
        if environment.backend().kind() != super::super::ExecutionBackendKind::Host {
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
    fn running_sessions(state: &JoinedProcessServiceState) -> usize {
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
    fn reclaim_finished_sessions(&self) {
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
            .ok_or_else(|| conflict_error(super::JOINED_STDIN_CLOSED))?;
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
    /// # Errors
    ///
    /// Returns the first join failure after attempting every active session.
    pub fn join_all(&self) -> Result<(), ExecutionBackendError> {
        let ids = lock(&self.inner.state)
            .active
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let deadline = Instant::now() + self.inner.limits.cleanup_duration();
        let mut failure = None;
        for id in ids {
            if Instant::now() >= deadline {
                failure.get_or_insert_with(cleanup_unresolved_error);
                break;
            }
            if let Err(error) = self.terminate_until(id, deadline) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn terminate_until(
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

    fn session(
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

    fn retire(&self, id: JoinedProcessSessionId, status: JoinedProcessStatus) {
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

fn displayed_command(command: &JoinedProcessCommand) -> String {
    let text = match command {
        JoinedProcessCommand::Shell { command, .. } => command.to_string(),
        JoinedProcessCommand::Literal {
            executable,
            arguments,
        } => format!("{} {}", executable.display(), arguments.join(" ")),
    };
    crate::internal::sanitize_job_command(&text)
}

pub(super) fn validate_absolute_program(
    executable: &Path,
    message: &'static str,
) -> Result<(), ExecutionBackendError> {
    if !executable.is_absolute() {
        return Err(invalid_request(message));
    }
    let value = executable
        .to_str()
        .ok_or_else(|| invalid_request("joined process executable is not portable UTF-8"))?;
    validate_process_value(value)
}

pub(super) fn validate_process_values(values: &[String]) -> Result<(), ExecutionBackendError> {
    if values.len() > MAX_LITERAL_PROCESS_ARGUMENTS {
        return Err(resource_error(
            "joined process argument count exceeds its bound",
        ));
    }
    values
        .iter()
        .try_for_each(|value| validate_process_value(value))
}

pub(super) fn validate_process_value(value: &str) -> Result<(), ExecutionBackendError> {
    if value.len() > MAX_EXECUTION_VALUE_BYTES || value.as_bytes().contains(&0) {
        return Err(invalid_request("joined process literal value is invalid"));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "spawn setup and every rollback edge stay with the single joined cleanup owner"
)]
fn start_joined_host_process(
    workspace: &ExecutionWorkspace,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
    limits: ExecutionEnvironmentLimits,
    queue_limit: usize,
    request: &JoinedProcessStartRequest,
    control: ExecutionOperationControl,
    pending_spool: Option<PendingJobOutput>,
) -> Result<JoinedSession, ExecutionBackendError> {
    match request.terminal_mode {
        JoinedProcessTerminalMode::Pipes => start_joined_pipes_process(
            workspace,
            process,
            environment,
            limits,
            queue_limit,
            request,
            control,
            pending_spool,
        ),
        JoinedProcessTerminalMode::UnixPty => start_joined_pty_process(
            workspace,
            process,
            environment,
            limits,
            queue_limit,
            request,
            control,
            pending_spool,
        ),
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "spawn setup and every rollback edge stay with the single joined cleanup owner"
)]
fn start_joined_pipes_process(
    workspace: &ExecutionWorkspace,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
    limits: ExecutionEnvironmentLimits,
    queue_limit: usize,
    request: &JoinedProcessStartRequest,
    control: ExecutionOperationControl,
    pending_spool: Option<PendingJobOutput>,
) -> Result<JoinedSession, ExecutionBackendError> {
    let cwd = resolve_existing_no_symlink(workspace, &request.cwd)?;
    if !cwd.is_dir() {
        return Err(workspace_error(
            "joined process cwd is not a contained directory",
        ));
    }
    let wrapped_shell = match &request.command {
        JoinedProcessCommand::Shell { command, .. } => {
            Some(restore_path_after_login(process, command)?)
        }
        JoinedProcessCommand::Literal { .. } => None,
    };
    let argv0 = match &request.command {
        JoinedProcessCommand::Shell { profile, .. } => Some(profile.executable()),
        JoinedProcessCommand::Literal { .. } => None,
    };
    let (program, arguments): (&Path, Vec<&str>) = match &request.command {
        JoinedProcessCommand::Literal {
            executable,
            arguments,
        } => (executable, arguments.iter().map(String::as_str).collect()),
        JoinedProcessCommand::Shell { profile, .. } => {
            let mut arguments = profile
                .arguments()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            arguments.push(wrapped_shell.as_deref().unwrap_or(""));
            (profile.revalidated_executable()?, arguments)
        }
    };
    let mut command = Command::new(program);
    command.args(arguments);
    command.arg0(argv0.unwrap_or(program));
    command
        .current_dir(cwd)
        .env_clear()
        .stdin(if request.keep_stdin_open || !request.stdin.is_empty() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (name, value) in environment {
        if name.as_str() == "PATH" {
            continue;
        }
        command.env(name.as_str(), value.0.as_ref());
    }
    command.env("PATH", host_process_path(process, environment)?);
    if let Some(home) = host_process_home(process, environment) {
        command.env("HOME", home);
    }
    let mut child = command
        .spawn()
        .map_err(|_| invalid_request("joined process executable could not be started"))?;
    let process_group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io_error("joined process returned an invalid pid"));
    let process_group = match process_group {
        Ok(process_group) => process_group,
        Err(error) => {
            return Err(
                terminate_direct_child(&mut child, limits.cleanup_duration())
                    .err()
                    .unwrap_or(error),
            );
        }
    };
    let mut stdin = child.stdin.take();
    if (request.keep_stdin_open || !request.stdin.is_empty()) && stdin.is_none() {
        return Err(cleanup_setup_error(
            &mut child,
            process_group,
            limits.cleanup_duration(),
            io_error("joined process stdin was unavailable"),
        ));
    }
    let Some(stdout) = child.stdout.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            process_group,
            limits.cleanup_duration(),
            io_error("joined process stdout was unavailable"),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            process_group,
            limits.cleanup_duration(),
            io_error("joined process stderr was unavailable"),
        ));
    };
    if let Some(stdin) = &stdin
        && let Err(error) = set_nonblocking(stdin)
    {
        return Err(cleanup_setup_error(
            &mut child,
            process_group,
            limits.cleanup_duration(),
            error,
        ));
    }

    let shared = Arc::new(Mutex::new(JoinedSessionShared {
        output: JoinedSessionOutput::default(),
        status: JoinedProcessStatus::Running,
        elapsed: None,
        terminal_changes: None,
        changes: tokio::sync::watch::channel(0).0,
        terminal: Arc::new(Condvar::new()),
        released: false,
        spool: None,
        pending_spool,
        leftover: None,
        disowned: false,
    }));
    let output_io_failed = Arc::new(AtomicBool::new(false));
    let io_stop = Arc::new(AtomicBool::new(false));
    let stdout_thread = spawn_joined_output_reader(
        stdout,
        JoinedProcessOutputStream::Stdout,
        Arc::clone(&shared),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    )
    .map_err(|error| {
        cleanup_setup_error(&mut child, process_group, limits.cleanup_duration(), error)
    })?;
    let stderr_thread = match spawn_joined_output_reader(
        stderr,
        JoinedProcessOutputStream::Stderr,
        Arc::clone(&shared),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    ) {
        Ok(thread) => thread,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            let deadline = Instant::now() + limits.cleanup_duration();
            let cleanup =
                terminate_owned_process_group_until(&mut child, process_group, None, deadline);
            let joined = join_thread_until(stdout_thread, deadline);
            return Err(match (cleanup, joined) {
                (Err(cleanup), _) => cleanup,
                (Ok(_), false) => cleanup_unresolved_error(),
                (Ok(_), true) => error,
            });
        }
    };
    let initial_input = match stdin.as_mut() {
        Some(stdin) => {
            write_joined_initial_input(stdin, &request.stdin, limits.execution_duration())
        }
        None => Ok(()),
    };
    if let Err(error) = initial_input {
        io_stop.store(true, Ordering::Release);
        let deadline = Instant::now() + limits.cleanup_duration();
        let cleanup =
            terminate_owned_process_group_until(&mut child, process_group, None, deadline);
        let joined = (
            join_thread_until(stdout_thread, deadline),
            join_thread_until(stderr_thread, deadline),
        );
        return Err(if let Err(cleanup) = cleanup {
            cleanup
        } else if !joined.0 || !joined.1 {
            cleanup_unresolved_error()
        } else {
            error
        });
    }
    let input = if request.keep_stdin_open { stdin } else { None };
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let foreground_control = control.clone();
    let promoted = Arc::new(AtomicU8::new(FOREGROUND));
    let thread_promoted = Arc::clone(&promoted);
    let stop_grace_millis = Arc::new(AtomicU64::new(2000));
    let thread_stop_grace = Arc::clone(&stop_grace_millis);
    let thread_shared = Arc::clone(&shared);
    let shell_command = matches!(request.command, JoinedProcessCommand::Shell { .. });
    let resources = JoinedProcessResources {
        child,
        process_group,
        stdout_thread: Some(stdout_thread),
        stderr_thread: Some(stderr_thread),
        io_stop,
        cleanup_duration: limits.cleanup_duration(),
        armed: true,
    };
    let thread = std::thread::Builder::new()
        .name(String::from("plak-joined-process"))
        .spawn(move || {
            let mut resources = resources;
            let (Some(stdout_thread), Some(stderr_thread)) = (
                resources.stdout_thread.take(),
                resources.stderr_thread.take(),
            ) else {
                let mut shared = lock(&thread_shared);
                shared.status = JoinedProcessStatus::Failed;
                shared.terminal_changed();
                return;
            };
            let process_group = resources.process_group;
            supervise_joined_process(
                &mut resources.child,
                process_group,
                stdout_thread,
                stderr_thread,
                limits,
                &control,
                &thread_promoted,
                &thread_stop_grace,
                &thread_stop,
                &resources.io_stop,
                &output_io_failed,
                &thread_shared,
                shell_command,
            );
            resources.armed = false;
        })
        .map_err(|_| io_error("joined process supervisor could not start"));
    let thread = match thread {
        Ok(thread) => thread,
        Err(error) => {
            stop.store(true, Ordering::Release);
            // Dropping the rejected closure runs JoinedProcessResources'
            // rollback owner, which terminates/reaps and joins both readers.
            return Err(error);
        }
    };
    Ok(JoinedSession {
        operation: Mutex::new(()),
        queued: AtomicUsize::new(0),
        queue_limit,
        input: Mutex::new(input.map(JoinedSessionInput::Pipes)),
        input_bytes: AtomicUsize::new(request.stdin.len()),
        input_timeout: limits.execution_duration(),
        cleanup_duration: limits.cleanup_duration(),
        shared,
        consumer_cursor: Arc::new(Mutex::new(JoinedProcessOutputCursor::new(0))),
        consumer_pending_utf8: Arc::new(Mutex::new(Vec::new())),
        consumer_quiet_polls: Arc::default(),
        consumed: AtomicBool::new(false),
        promoted,
        foreground_control,
        stop_grace_millis,
        command: displayed_command(&request.command),
        started_at: plakous_model::Timestamp::new(plakous_model::now_unix_secs(), 0)
            .map_err(|_| invalid_request("process start timestamp is invalid"))?,
        started: Instant::now(),
        ended: Mutex::new(None),
        stopped_by: Mutex::new(None),
        owner: Mutex::new(Some(HostProcessOwner {
            stop,
            thread: Some(thread),
        })),
    })
}

struct PtyMasterDescriptor<'a> {
    raw: RawFd,
    _master: std::marker::PhantomData<&'a dyn MasterPty>,
}

impl AsRawFd for PtyMasterDescriptor<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.raw
    }
}

fn clone_linux_pty_master(master: &dyn MasterPty) -> Result<fs::File, ExecutionBackendError> {
    let raw = master
        .as_raw_fd()
        .ok_or_else(|| io_error("joined process PTY master identity was unavailable"))?;
    filedescriptor::FileDescriptor::dup(&PtyMasterDescriptor {
        raw,
        _master: std::marker::PhantomData,
    })
    .and_then(|descriptor| descriptor.as_file())
    .map_err(|_| io_error("joined process PTY master could not be safely duplicated"))
}

fn joined_pty_command(
    program: &Path,
    argv0: Option<&Path>,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> CommandBuilder {
    // portable-pty couples the executable to argv[0], so the revalidated
    // enrolled alias is the program name. No extra shell is required.
    let mut command = CommandBuilder::new(argv0.unwrap_or(program));
    command.args(arguments);
    command
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "PTY spawn and every rollback edge stay with the single joined cleanup owner"
)]
fn start_joined_pty_process(
    workspace: &ExecutionWorkspace,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
    limits: ExecutionEnvironmentLimits,
    queue_limit: usize,
    request: &JoinedProcessStartRequest,
    control: ExecutionOperationControl,
    pending_spool: Option<PendingJobOutput>,
) -> Result<JoinedSession, ExecutionBackendError> {
    let cwd = resolve_existing_no_symlink(workspace, &request.cwd)?;
    if !cwd.is_dir() {
        return Err(workspace_error(
            "joined process cwd is not a contained directory",
        ));
    }
    let wrapped_shell = match &request.command {
        JoinedProcessCommand::Shell { command, .. } => Some(format!(
            "trap '' HUP; {}",
            restore_path_after_login(process, command)?
        )),
        JoinedProcessCommand::Literal { .. } => None,
    };
    let argv0 = match &request.command {
        JoinedProcessCommand::Shell { profile, .. } => Some(profile.executable()),
        JoinedProcessCommand::Literal { .. } => None,
    };
    let (program, arguments): (&Path, Vec<&str>) = match &request.command {
        JoinedProcessCommand::Literal {
            executable,
            arguments,
        } => (executable, arguments.iter().map(String::as_str).collect()),
        JoinedProcessCommand::Shell { profile, .. } => {
            let mut arguments = profile
                .arguments()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            arguments.push(wrapped_shell.as_deref().unwrap_or(""));
            (profile.revalidated_executable()?, arguments)
        }
    };
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: JOINED_PROCESS_PTY_INITIAL_ROWS,
            cols: JOINED_PROCESS_PTY_INITIAL_COLUMNS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|_| io_error("joined process PTY could not be opened"))?;
    let portable_pty::PtyPair { master, slave } = pair;
    let mut command = joined_pty_command(program, argv0, arguments);
    command.cwd(&cwd);
    command.env_clear();
    for (name, value) in environment {
        if name.as_str() == "PATH" {
            continue;
        }
        command.env(name.as_str(), value.0.as_ref());
    }
    command.env("PATH", host_process_path(process, environment)?);
    if let Some(home) = host_process_home(process, environment) {
        command.env("HOME", home);
    }
    // The session's terminal is a duplicate of the master descriptor, and the
    // duplicate is what keeps the master side open. The original is dropped
    // with this function; closing one of two duplicates closes neither end.
    let terminal = clone_linux_pty_master(master.as_ref())?;
    set_nonblocking(&terminal)?;
    let reader = terminal
        .try_clone()
        .map_err(|_| io_error("joined process PTY reader was unavailable"))?;
    let writer = terminal;
    let mut child = slave
        .spawn_command(command)
        .map_err(|_| invalid_request("joined process PTY command could not be started"))?;
    drop(slave);
    let process_group = child
        .process_id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io_error("joined process PTY returned an invalid pid"));
    let process_group = match process_group {
        Ok(process_group) => process_group,
        Err(error) => {
            return Err(
                terminate_direct_pty_child(child.as_mut(), limits.cleanup_duration())
                    .err()
                    .unwrap_or(error),
            );
        }
    };
    let shared = Arc::new(Mutex::new(JoinedSessionShared {
        output: JoinedSessionOutput::default(),
        status: JoinedProcessStatus::Running,
        elapsed: None,
        terminal_changes: None,
        changes: tokio::sync::watch::channel(0).0,
        terminal: Arc::new(Condvar::new()),
        released: false,
        spool: None,
        pending_spool,
        leftover: None,
        disowned: false,
    }));
    let output_io_failed = Arc::new(AtomicBool::new(false));
    let io_stop = Arc::new(AtomicBool::new(false));
    let reader_thread = match spawn_joined_output_reader(
        reader,
        JoinedProcessOutputStream::Terminal,
        Arc::clone(&shared),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    ) {
        Ok(thread) => thread,
        Err(error) => {
            return Err(cleanup_pty_setup_error(
                child.as_mut(),
                process_group,
                limits.cleanup_duration(),
                error,
            ));
        }
    };
    let (input, input_owner, input_thread) = match spawn_pty_input_writer(Box::new(writer)) {
        Ok(input) => input,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            let deadline = Instant::now() + limits.cleanup_duration();
            let cleanup = cleanup_pty_setup_error(
                child.as_mut(),
                process_group,
                limits.cleanup_duration(),
                error,
            );
            return Err(if join_thread_until(reader_thread, deadline) {
                cleanup
            } else {
                cleanup_unresolved_error()
            });
        }
    };
    if let Err(error) = input.write(&request.stdin, limits.execution_duration()) {
        io_stop.store(true, Ordering::Release);
        let deadline = Instant::now() + limits.cleanup_duration();
        let cleanup = cleanup_pty_setup_error(
            child.as_mut(),
            process_group,
            limits.cleanup_duration(),
            error,
        );
        let _ = input_owner.try_send(PtyInputCommand::Stop);
        let joined = (
            join_thread_until(reader_thread, deadline),
            join_thread_until(input_thread, deadline),
        );
        return Err(if !joined.0 || !joined.1 {
            cleanup_unresolved_error()
        } else {
            cleanup
        });
    }
    let input = if request.keep_stdin_open {
        Some(JoinedSessionInput::Pty(input))
    } else {
        if let Err(error) = input.close(limits.execution_duration()) {
            io_stop.store(true, Ordering::Release);
            let deadline = Instant::now() + limits.cleanup_duration();
            let cleanup = cleanup_pty_setup_error(
                child.as_mut(),
                process_group,
                limits.cleanup_duration(),
                error,
            );
            let _ = input_owner.try_send(PtyInputCommand::Stop);
            let joined = (
                join_thread_until(reader_thread, deadline),
                join_thread_until(input_thread, deadline),
            );
            return Err(if !joined.0 || !joined.1 {
                cleanup_unresolved_error()
            } else {
                cleanup
            });
        }
        None
    };
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let foreground_control = control.clone();
    let promoted = Arc::new(AtomicU8::new(FOREGROUND));
    let thread_promoted = Arc::clone(&promoted);
    let stop_grace_millis = Arc::new(AtomicU64::new(2000));
    let thread_stop_grace = Arc::clone(&stop_grace_millis);
    let thread_shared = Arc::clone(&shared);
    let shell_command = matches!(request.command, JoinedProcessCommand::Shell { .. });
    let resources = JoinedPtyResources {
        child,
        process_group,
        reader_thread: Some(reader_thread),
        input_thread: Some(input_thread),
        input_owner,
        io_stop,
        cleanup_duration: limits.cleanup_duration(),
        armed: true,
    };
    let thread = std::thread::Builder::new()
        .name(String::from("plak-joined-pty"))
        .spawn(move || {
            let mut resources = resources;
            let (Some(reader_thread), Some(input_thread)) = (
                resources.reader_thread.take(),
                resources.input_thread.take(),
            ) else {
                let mut shared = lock(&thread_shared);
                shared.status = JoinedProcessStatus::Failed;
                shared.terminal_changed();
                return;
            };
            supervise_joined_pty(
                resources.child.as_mut(),
                resources.process_group,
                reader_thread,
                input_thread,
                &resources.input_owner,
                limits,
                &control,
                &thread_promoted,
                &thread_stop_grace,
                &thread_stop,
                &resources.io_stop,
                &output_io_failed,
                &thread_shared,
                shell_command,
            );
            resources.armed = false;
        })
        .map_err(|_| io_error("joined process PTY supervisor could not start"))?;
    Ok(JoinedSession {
        operation: Mutex::new(()),
        queued: AtomicUsize::new(0),
        queue_limit,
        input: Mutex::new(input),
        input_bytes: AtomicUsize::new(request.stdin.len()),
        input_timeout: limits.execution_duration(),
        cleanup_duration: limits.cleanup_duration(),
        shared,
        consumer_cursor: Arc::new(Mutex::new(JoinedProcessOutputCursor::new(0))),
        consumer_pending_utf8: Arc::new(Mutex::new(Vec::new())),
        consumer_quiet_polls: Arc::default(),
        consumed: AtomicBool::new(false),
        promoted,
        foreground_control,
        stop_grace_millis,
        command: displayed_command(&request.command),
        started_at: plakous_model::Timestamp::new(plakous_model::now_unix_secs(), 0)
            .map_err(|_| invalid_request("process start timestamp is invalid"))?,
        started: Instant::now(),
        ended: Mutex::new(None),
        stopped_by: Mutex::new(None),
        owner: Mutex::new(Some(HostProcessOwner {
            stop,
            thread: Some(thread),
        })),
    })
}

fn write_joined_initial_input(
    stdin: &mut std::process::ChildStdin,
    bytes: &[u8],
    duration: Duration,
) -> Result<(), ExecutionBackendError> {
    write_joined_input(stdin, bytes, duration)
}

fn write_joined_input(
    stdin: &mut std::process::ChildStdin,
    bytes: &[u8],
    duration: Duration,
) -> Result<(), ExecutionBackendError> {
    let deadline = Instant::now() + duration;
    let mut written = 0;
    let mut blocked_since = Instant::now();
    while written < bytes.len() {
        if Instant::now() >= deadline {
            return Err(backend_error(
                ExecutionBackendErrorKind::TimedOut,
                "joined process initial input timed out",
            ));
        }
        match stdin.write(&bytes[written..]) {
            Ok(0) => return Err(io_error("joined process stdin closed during initial input")),
            Ok(count) => {
                written = written.saturating_add(count);
                blocked_since = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                sleep_until_next_poll(blocked_since, deadline);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(io_error("joined process initial input failed")),
        }
    }
    Ok(())
}

fn write_joined_session_input(
    input: &mut JoinedSessionInput,
    bytes: &[u8],
    duration: Duration,
) -> Result<(), ExecutionBackendError> {
    match input {
        JoinedSessionInput::Pipes(stdin) => {
            write_joined_input(stdin, bytes, duration)?;
            stdin
                .flush()
                .map_err(|_| io_error("joined process stdin flush failed"))
        }
        JoinedSessionInput::Pty(sender) => sender.write(bytes, duration),
    }
}

fn close_joined_session_input(
    input: &mut JoinedSessionInput,
    duration: Duration,
) -> Result<(), ExecutionBackendError> {
    match input {
        JoinedSessionInput::Pipes(_) => Ok(()),
        JoinedSessionInput::Pty(sender) => sender.close(duration),
    }
}

enum PtyInputCommand {
    Write(Vec<u8>, SyncSender<Result<(), ()>>),
    Close(SyncSender<Result<(), ()>>),
    Stop,
}

struct PtyInputSender {
    commands: SyncSender<PtyInputCommand>,
}

impl PtyInputSender {
    fn write(&self, bytes: &[u8], duration: Duration) -> Result<(), ExecutionBackendError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let (acknowledge, acknowledged) = sync_channel(1);
        self.commands
            .try_send(PtyInputCommand::Write(bytes.to_vec(), acknowledge))
            .map_err(|error| match error {
                TrySendError::Full(_) => resource_error("joined process PTY input queue is full"),
                TrySendError::Disconnected(_) => {
                    conflict_error("joined process PTY input is closed")
                }
            })?;
        acknowledged
            .recv_timeout(duration)
            .map_err(|_| io_error("joined process PTY input did not complete within its bound"))?
            .map_err(|()| io_error("joined process PTY input failed"))
    }

    fn close(&self, duration: Duration) -> Result<(), ExecutionBackendError> {
        let (acknowledge, acknowledged) = sync_channel(1);
        self.commands
            .try_send(PtyInputCommand::Close(acknowledge))
            .map_err(|error| match error {
                TrySendError::Full(_) => resource_error("joined process PTY input queue is full"),
                TrySendError::Disconnected(_) => {
                    conflict_error("joined process PTY input is closed")
                }
            })?;
        acknowledged
            .recv_timeout(duration)
            .map_err(|_| io_error("joined process PTY EOF did not complete within its bound"))?
            .map_err(|()| io_error("joined process PTY EOF failed"))
    }
}

fn spawn_pty_input_writer(
    mut writer: Box<dyn Write + Send>,
) -> Result<
    (
        PtyInputSender,
        SyncSender<PtyInputCommand>,
        std::thread::JoinHandle<()>,
    ),
    ExecutionBackendError,
> {
    let (commands, incoming): (SyncSender<PtyInputCommand>, Receiver<PtyInputCommand>) =
        sync_channel(1);
    let owner_commands = commands.clone();
    let thread = std::thread::Builder::new()
        .name(String::from("plak-joined-pty-input"))
        .spawn(move || {
            while let Ok(command) = incoming.recv() {
                match command {
                    PtyInputCommand::Write(bytes, acknowledge) => {
                        let result = writer
                            .write_all(&bytes)
                            .and_then(|()| writer.flush())
                            .map_err(|_| ());
                        let failed = result.is_err();
                        let _ = acknowledge.send(result);
                        if failed {
                            return;
                        }
                    }
                    PtyInputCommand::Close(acknowledge) => {
                        let result = writer
                            .write_all(b"\n\x04")
                            .and_then(|()| writer.flush())
                            .map_err(|_| ());
                        let _ = acknowledge.send(result);
                        return;
                    }
                    PtyInputCommand::Stop => return,
                }
            }
        })
        .map_err(|_| io_error("joined process PTY input worker could not start"))?;
    Ok((PtyInputSender { commands }, owner_commands, thread))
}

struct JoinedProcessResources {
    child: std::process::Child,
    process_group: rustix::process::Pid,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
    io_stop: Arc<AtomicBool>,
    cleanup_duration: Duration,
    armed: bool,
}

impl Drop for JoinedProcessResources {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let deadline = Instant::now() + self.cleanup_duration;
        let _ = terminate_owned_process_group_until(
            &mut self.child,
            self.process_group,
            None,
            deadline,
        );
        self.io_stop.store(true, Ordering::Release);
        if let Some(thread) = self.stdout_thread.take() {
            let _ = join_thread_until(thread, deadline);
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = join_thread_until(thread, deadline);
        }
    }
}

struct JoinedPtyResources {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    process_group: rustix::process::Pid,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    input_thread: Option<std::thread::JoinHandle<()>>,
    input_owner: SyncSender<PtyInputCommand>,
    io_stop: Arc<AtomicBool>,
    cleanup_duration: Duration,
    armed: bool,
}

impl Drop for JoinedPtyResources {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let deadline = Instant::now() + self.cleanup_duration;
        let _ = terminate_owned_pty_group_until(
            self.child.as_mut(),
            self.process_group,
            None,
            deadline,
        );
        self.io_stop.store(true, Ordering::Release);
        let _ = self.input_owner.try_send(PtyInputCommand::Stop);
        if let Some(thread) = self.reader_thread.take() {
            let _ = join_thread_until(thread, deadline);
        }
        if let Some(thread) = self.input_thread.take() {
            let _ = join_thread_until(thread, deadline);
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the PTY supervisor explicitly owns every process, I/O, limit, and cancellation input"
)]
fn supervise_joined_pty(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    reader_thread: std::thread::JoinHandle<()>,
    input_thread: std::thread::JoinHandle<()>,
    input_owner: &SyncSender<PtyInputCommand>,
    limits: ExecutionEnvironmentLimits,
    control: &ExecutionOperationControl,
    promoted: &AtomicU8,
    stop_grace_millis: &AtomicU64,
    stopped: &AtomicBool,
    io_stop: &AtomicBool,
    output_io_failed: &AtomicBool,
    shared: &Mutex<JoinedSessionShared>,
    shell_command: bool,
) {
    let started = Instant::now();
    let mut status = None;
    let mut group = GroupProbe::new(process_group);
    let mut disowned = false;
    let reason = loop {
        if stopped.load(Ordering::Acquire) {
            break JoinedProcessStatus::Terminated;
        }
        if lock(shared).disowned {
            disowned = true;
            break JoinedProcessStatus::OutcomeUnknown;
        }
        if cancel_foreground(promoted, control) {
            break JoinedProcessStatus::Cancelled;
        }
        if output_io_failed.load(Ordering::Acquire) {
            break JoinedProcessStatus::Failed;
        }
        if started.elapsed() >= limits.session_duration() {
            break JoinedProcessStatus::TimedOut;
        }
        match peek_leader_exit(process_group) {
            Ok(Some(exit)) => {
                if shell_command && observe_leftover_group(&mut group, exit.code, shared) {
                    std::thread::sleep(supervision_poll_interval(started.elapsed()));
                    continue;
                }
                match child.try_wait() {
                    Ok(Some(exit)) => {
                        let code = i32::try_from(exit.exit_code()).ok();
                        status = Some(exit);
                        break JoinedProcessStatus::Exited(code);
                    }
                    Ok(None) => {}
                    Err(_) => break JoinedProcessStatus::Failed,
                }
            }
            Ok(None) => std::thread::sleep(supervision_poll_interval(started.elapsed())),
            Err(_) => break JoinedProcessStatus::Failed,
        }
    };
    let deadline = Instant::now() + limits.cleanup_duration();
    let mut cleanup = if disowned {
        Ok(())
    } else {
        terminate_owned_pty_group_with_grace(
            child,
            process_group,
            status,
            deadline,
            Duration::from_millis(stop_grace_millis.load(Ordering::Acquire)),
        )
        .map(|_| ())
    };
    let draining = Instant::now();
    if !disowned && matches!(reason, JoinedProcessStatus::Exited(_)) && cleanup.is_ok() {
        while Instant::now() < deadline && !reader_thread.is_finished() {
            sleep_until_next_poll(draining, deadline);
        }
    }
    io_stop.store(true, Ordering::Release);
    let _ = input_owner.try_send(PtyInputCommand::Stop);
    while Instant::now() < deadline && (!reader_thread.is_finished() || !input_thread.is_finished())
    {
        sleep_until_next_poll(draining, deadline);
    }
    let reader_joined = join_thread_until(reader_thread, deadline);
    let input_joined = join_thread_until(input_thread, deadline);
    let workers_joined = reader_joined && input_joined;
    if disowned {
        cleanup = if workers_joined && !output_io_failed.load(Ordering::Acquire) {
            child
                .try_wait()
                .map(|_| ())
                .map_err(|_| io_error("released process leader could not be reaped"))
        } else {
            terminate_owned_pty_group_with_grace(
                child,
                process_group,
                None,
                deadline,
                Duration::from_millis(200),
            )
            .map(|_| ())
        };
    }
    let final_status = joined_process_final_status(
        reason,
        cleanup.is_ok(),
        workers_joined,
        output_io_failed.load(Ordering::Acquire),
    );
    let mut shared = lock(shared);
    shared.status = final_status;
    shared.elapsed = Some(started.elapsed());
    shared.terminal_changed();
}

#[allow(
    clippy::too_many_arguments,
    reason = "the supervisor explicitly owns every process, I/O, limit, and cancellation input"
)]
fn supervise_joined_process(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    stdout_thread: std::thread::JoinHandle<()>,
    stderr_thread: std::thread::JoinHandle<()>,
    limits: ExecutionEnvironmentLimits,
    control: &ExecutionOperationControl,
    promoted: &AtomicU8,
    stop_grace_millis: &AtomicU64,
    stopped: &AtomicBool,
    io_stop: &AtomicBool,
    output_io_failed: &AtomicBool,
    shared: &Mutex<JoinedSessionShared>,
    shell_command: bool,
) {
    let started = Instant::now();
    let mut status = None;
    let mut group = GroupProbe::new(process_group);
    let mut disowned = false;
    let reason = loop {
        if stopped.load(Ordering::Acquire) {
            break JoinedProcessStatus::Terminated;
        }
        if lock(shared).disowned {
            disowned = true;
            break JoinedProcessStatus::OutcomeUnknown;
        }
        if cancel_foreground(promoted, control) {
            break JoinedProcessStatus::Cancelled;
        }
        if output_io_failed.load(Ordering::Acquire) {
            break JoinedProcessStatus::Failed;
        }
        if started.elapsed() >= limits.session_duration() {
            break JoinedProcessStatus::TimedOut;
        }
        match peek_leader_exit(process_group) {
            Ok(Some(exit)) => {
                if shell_command && observe_leftover_group(&mut group, exit.code, shared) {
                    std::thread::sleep(supervision_poll_interval(started.elapsed()));
                    continue;
                }
                match child.try_wait() {
                    Ok(Some(exit)) => {
                        let code = exit.code();
                        status = Some(exit);
                        break JoinedProcessStatus::Exited(code);
                    }
                    Ok(None) => {}
                    Err(_) => break JoinedProcessStatus::Failed,
                }
            }
            Ok(None) => std::thread::sleep(supervision_poll_interval(started.elapsed())),
            Err(_) => break JoinedProcessStatus::Failed,
        }
    };
    let deadline = Instant::now() + limits.cleanup_duration();
    let mut cleanup = if disowned {
        Ok(())
    } else {
        terminate_owned_process_group_with_grace(
            child,
            process_group,
            status,
            deadline,
            Duration::from_millis(stop_grace_millis.load(Ordering::Acquire)),
        )
        .map(|_| ())
    };
    if !disowned && matches!(reason, JoinedProcessStatus::Exited(_)) && cleanup.is_ok() {
        wait_for_output_readers_until(&stdout_thread, &stderr_thread, deadline);
    }
    io_stop.store(true, Ordering::Release);
    let stdout_joined = join_thread_until(stdout_thread, deadline);
    let stderr_joined = join_thread_until(stderr_thread, deadline);
    let readers_joined = stdout_joined && stderr_joined;
    if disowned {
        cleanup = if readers_joined && !output_io_failed.load(Ordering::Acquire) {
            child
                .try_wait()
                .map(|_| ())
                .map_err(|_| io_error("released process leader could not be reaped"))
        } else {
            terminate_owned_process_group_with_grace(
                child,
                process_group,
                None,
                deadline,
                Duration::from_millis(200),
            )
            .map(|_| ())
        };
    }
    let final_status = joined_process_final_status(
        reason,
        cleanup.is_ok(),
        readers_joined,
        output_io_failed.load(Ordering::Acquire),
    );
    let mut shared = lock(shared);
    shared.status = final_status;
    shared.elapsed = Some(started.elapsed());
    shared.terminal_changed();
}

const fn joined_process_final_status(
    reason: JoinedProcessStatus,
    cleanup_resolved: bool,
    workers_joined: bool,
    output_io_failed: bool,
) -> JoinedProcessStatus {
    if !cleanup_resolved || !workers_joined {
        JoinedProcessStatus::CleanupUnresolved
    } else if output_io_failed {
        JoinedProcessStatus::Failed
    } else {
        reason
    }
}

fn spawn_joined_output_reader<R>(
    reader: R,
    stream: JoinedProcessOutputStream,
    shared: Arc<Mutex<JoinedSessionShared>>,
    io_failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    limits: ExecutionEnvironmentLimits,
) -> Result<std::thread::JoinHandle<()>, ExecutionBackendError>
where
    R: AsFd + Read + Send + 'static,
{
    set_nonblocking(&reader)?;
    std::thread::Builder::new()
        .name(String::from("plak-joined-output"))
        .spawn(move || {
            let mut reader = reader;
            let mut chunk = [0_u8; 16 * 1024];
            loop {
                if lock(&shared).disowned {
                    if detach_output_reader(&reader, stream, &shared).is_err() {
                        io_failed.store(true, Ordering::Release);
                    }
                    return;
                }
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let read = match reader.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(read) => read,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if stream == JoinedProcessOutputStream::Terminal
                            && error.raw_os_error() == Some(Errno::IO.raw_os_error()) =>
                    {
                        return;
                    }
                    Err(_) => {
                        io_failed.store(true, Ordering::Release);
                        return;
                    }
                };
                if stop.load(Ordering::Acquire) && !lock(&shared).disowned {
                    return;
                }
                let mut state = lock(&shared);
                state
                    .changes
                    .send_modify(|revision| *revision = revision.wrapping_add(1));
                let read_bytes = u64::try_from(read).unwrap_or(u64::MAX);
                let sequence = state.output.next_sequence;
                state.output.next_sequence = sequence.saturating_add(1);
                let cursor = state.output.next_cursor;
                state.output.next_cursor = cursor.saturating_add(read_bytes);
                state.output.retained_bytes =
                    state.output.retained_bytes.saturating_add(read_bytes);
                match stream {
                    JoinedProcessOutputStream::Stdout | JoinedProcessOutputStream::Terminal => {
                        state.output.total_stdout_bytes =
                            state.output.total_stdout_bytes.saturating_add(read_bytes);
                    }
                    JoinedProcessOutputStream::Stderr => {
                        state.output.total_stderr_bytes =
                            state.output.total_stderr_bytes.saturating_add(read_bytes);
                    }
                }
                state.output.records.push_back(JoinedProcessOutputRecord {
                    sequence,
                    cursor,
                    stream,
                    bytes: Arc::from(&chunk[..read]),
                });
                if let Some(spool) = &mut state.spool {
                    spool.append(&chunk[..read]);
                } else {
                    state.maybe_spool(&chunk[..read], false);
                }
                // A session keeps a bounded window of its newest output rather
                // than dying at a byte count: a long build's failure is at the
                // end. Evicted bytes stay counted in the cursor space, so a
                // reader behind the window learns exactly how much it missed.
                let window = limits.max_combined_output_bytes();
                while state.output.retained_bytes > window {
                    let excess = state.output.retained_bytes - window;
                    let Some(front) = state.output.records.pop_front() else {
                        break;
                    };
                    let front_bytes = u64::try_from(front.bytes.len()).unwrap_or(u64::MAX);
                    if front_bytes > excess {
                        let keep_from = usize::try_from(excess).unwrap_or(front.bytes.len());
                        state.output.records.push_front(JoinedProcessOutputRecord {
                            sequence: front.sequence,
                            cursor: front.cursor.saturating_add(excess),
                            stream: front.stream,
                            bytes: Arc::from(&front.bytes[keep_from..]),
                        });
                        state.output.retained_bytes = window;
                    } else {
                        state.output.retained_bytes -= front_bytes;
                    }
                }
            }
        })
        .map_err(|_| io_error("joined process output worker could not start"))
}

fn detach_output_reader(
    reader: &impl AsFd,
    stream: JoinedProcessOutputStream,
    shared: &Mutex<JoinedSessionShared>,
) -> Result<(), ExecutionBackendError> {
    let input = rustix::io::dup(reader)
        .map(fs::File::from)
        .map_err(|_| io_error("released output could not be transferred"))?;
    let flags = rustix::fs::fcntl_getfl(&input)
        .map_err(|_| io_error("released output flags unavailable"))?;
    rustix::fs::fcntl_setfl(&input, flags & !OFlags::NONBLOCK)
        .map_err(|_| io_error("released output could not become blocking"))?;
    let (output, room) = lock(shared)
        .spool
        .as_mut()
        .and_then(|spool| {
            spool.detached_output(if stream == JoinedProcessOutputStream::Terminal {
                1
            } else {
                2
            })
        })
        .map_or_else(
            || (Stdio::null(), 0),
            |(file, room)| (Stdio::from(file), room),
        );
    // A thread would die with `plak run`. This deliberately unowned, EOF-bound
    // drain process keeps the service's pipe readable after the product exits.
    let mut drain = Command::new("/bin/sh")
        .args([
            "-c",
            "trap '' HUP; head -c \"$1\"; exec cat >/dev/null",
            "released-output",
            &room.to_string(),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .process_group(0)
        .stdin(input)
        .stdout(output)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| io_error("released output drainer could not start"))?;
    std::thread::Builder::new()
        .name(String::from("plak-released-output"))
        .spawn(move || {
            let _ = drain.wait();
        })
        .map_err(|_| io_error("released output drainer could not be reaped"))?;
    Ok(())
}

/// Minimal PATH used when the prepared request injects no PATH and the host
/// process itself has none.
const DEFAULT_HOST_PROCESS_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// Captured process authority shared by literal, pipes and PTY execution.
pub(in crate::environment) struct HostProcessContext {
    pub(super) fallback_path: String,
    pub(super) home: Option<PathBuf>,
    pub(super) prefix: Option<(PathBuf, fs::File)>,
}

impl HostProcessContext {
    pub(in crate::environment) fn capture(
        workspace: &crate::WorkspacePolicy,
        options: super::super::HostProcessOptions,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let invalid = || {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::Workspace,
                "required process PATH prefix is unavailable or outside the workspace",
            )
        };
        let prefix = options
            .required_path_prefix
            .map(|relative| {
                let path = workspace.root().join(relative);
                let canonical = fs::canonicalize(&path).map_err(|_| invalid())?;
                if canonical != path
                    || !canonical.starts_with(workspace.root())
                    || !canonical.is_dir()
                    || canonical.to_str().is_none_or(|path| path.contains(':'))
                {
                    return Err(invalid());
                }
                let directory = fs::File::open(&canonical).map_err(|_| invalid())?;
                Ok((canonical, directory))
            })
            .transpose()?;
        let fallback_path = std::env::var("PATH")
            .ok()
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= MAX_EXECUTION_VALUE_BYTES
                    && !value.contains('\0')
            })
            .unwrap_or_else(|| DEFAULT_HOST_PROCESS_PATH.to_owned());
        let home = captured_process_home(std::env::var_os("HOME").map(PathBuf::from));
        Ok(Self {
            fallback_path,
            home,
            prefix,
        })
    }

    fn required_prefix(&self) -> Result<Option<&Path>, ExecutionBackendError> {
        let Some((path, directory)) = &self.prefix else {
            return Ok(None);
        };
        let captured = directory
            .metadata()
            .map_err(|_| invalid_request("required process PATH prefix is unavailable"))?;
        let current = fs::symlink_metadata(path)
            .map_err(|_| invalid_request("required process PATH prefix is unavailable"))?;
        if !current.is_dir()
            || current.dev() != captured.dev()
            || current.ino() != captured.ino()
            || fs::canonicalize(path).ok().as_ref() != Some(path)
        {
            return Err(invalid_request(
                "required process PATH prefix identity changed",
            ));
        }
        Ok(Some(path))
    }
}

fn captured_process_home(home: Option<PathBuf>) -> Option<PathBuf> {
    home.filter(|home| home.is_absolute())
        .map(|home| fs::canonicalize(&home).unwrap_or(home))
        .filter(|home| home.is_dir() && home != Path::new("/"))
}

pub(super) fn host_process_path(
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
) -> Result<String, ExecutionBackendError> {
    let base = environment
        .iter()
        .find(|(name, value)| name.as_str() == "PATH" && !value.0.is_empty())
        .map_or(process.fallback_path.as_str(), |(_, value)| {
            value.0.as_ref()
        });
    Ok(match process.required_prefix()? {
        Some(prefix) => format!("{}:{base}", prefix.display()),
        None => base.to_owned(),
    })
}

/// The `HOME` a host spawn should carry, or `None` when it must not be set.
///
/// A curated `HOME` in the descriptor's environment wins; the captured host
/// home is a runtime fallback only, so an `env_clear`ed child never inherits an
/// empty `HOME`. Bash reads `~/.profile` from the passwd entry whether or not
/// `HOME` is set, so an unset `HOME` breaks every `$HOME`-relative profile line
/// and every tool that resolves `$HOME` inside the model's shell.
pub(super) fn host_process_home<'context>(
    process: &'context HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
) -> Option<&'context Path> {
    if environment
        .iter()
        .any(|(name, value)| name.as_str() == "HOME" && !value.0.is_empty())
    {
        return None;
    }
    process.home.as_deref()
}

/// Restore the required prefix after a login shell initializes its own base PATH.
fn restore_path_after_login(
    process: &HostProcessContext,
    command: &str,
) -> Result<String, ExecutionBackendError> {
    let Some(prefix) = process.required_prefix()? else {
        return Ok(command.to_owned());
    };
    let prefix = prefix.display().to_string().replace('\'', "'\\''");
    Ok(format!("PATH='{prefix}':\"$PATH\"; export PATH\n{command}"))
}

/// Resolves `program` to an absolute executable path.
///
/// Absolute paths are accepted as-is (existence is checked at spawn). Basename
/// programs are looked up on the prepared PATH or the default host PATH so
/// models can call `true` / `printf` without absolute paths. Relative paths
/// containing `/` remain rejected (no workspace-relative program resolution).
pub(super) fn resolve_host_process_program(
    program: &str,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
) -> Result<PathBuf, ExecutionBackendError> {
    let path = Path::new(program);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if program.contains('/') || program.contains('\\') {
        return Err(invalid_request(
            "host literal process program must be absolute or a PATH basename",
        ));
    }
    if program.is_empty() {
        return Err(invalid_request(
            "literal process program was not found on the host PATH",
        ));
    }
    let path_value = host_process_path(process, environment)?;
    for directory in path_value.split(':') {
        if directory.is_empty() {
            continue;
        }
        let candidate = Path::new(directory).join(program);
        // execvp semantics: a non-executable file of the same name earlier on
        // PATH (uv installs a sourceable `~/.local/bin/env`) does not shadow
        // the real program later on PATH.
        if candidate.is_file()
            && rustix::fs::access(&candidate, rustix::fs::Access::EXEC_OK).is_ok()
        {
            return Ok(candidate);
        }
    }
    Err(invalid_request(
        "literal process program was not found on the host PATH",
    ))
}

pub(super) fn execute_host_literal_process(
    workspace: ExecutionWorkspace,
    request: PreparedLiteralProcess,
    limits: ExecutionEnvironmentLimits,
    control: ExecutionOperationControl,
    process: Arc<HostProcessContext>,
) -> ExecutionBackendFuture<LiteralProcessResult> {
    Box::pin(async move {
        control.check_cancelled()?;
        let program = resolve_host_process_program(
            request.request.program(),
            &process,
            request.environment.as_ref(),
        )?;
        let resolved_cwd = resolve_existing_no_symlink(&workspace, request.request.cwd())?;
        if !resolved_cwd.is_dir() {
            return Err(workspace_error(
                "literal process cwd is not a contained directory",
            ));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name(String::from("plak-host-process"))
            .spawn(move || {
                let result = supervise_host_process(
                    &process,
                    resolved_cwd,
                    &program,
                    &request,
                    limits,
                    &control,
                    &thread_stop,
                );
                let _ = sender.send(result);
            })
            .map_err(|_| io_error("host process supervisor could not start"))?;
        let mut owner = HostProcessOwner {
            stop,
            thread: Some(thread),
        };
        let result = receiver
            .await
            .map_err(|_| io_error("host process supervisor stopped without a result"))?;
        if !owner.join_until(Instant::now() + limits.cleanup_duration())? {
            return Err(cleanup_unresolved_error());
        }
        result
    })
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "process setup rollback and joined cleanup are kept in one auditable supervisor"
)]
fn supervise_host_process(
    process: &HostProcessContext,
    cwd: PathBuf,
    program: &Path,
    request: &PreparedLiteralProcess,
    limits: ExecutionEnvironmentLimits,
    control: &ExecutionOperationControl,
    dropped: &AtomicBool,
) -> Result<LiteralProcessResult, ExecutionBackendError> {
    let started = Instant::now();
    let mut command = Command::new(program);
    command.args(request.request.arguments());
    command
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // Host processes intentionally clear the ambient environment. Inject a
    // minimal PATH so child processes inherit a usable default lookup path.
    // Full host credential/env inheritance remains opt-in via delegations.
    // The explicit required workspace prefix is revalidated before spawn.
    for (name, value) in request.environment.iter() {
        if name.as_str() == "PATH" {
            continue;
        }
        command.env(name.as_str(), value.0.as_ref());
    }
    command.env(
        "PATH",
        host_process_path(process, request.environment.as_ref())?,
    );
    if let Some(home) = host_process_home(process, request.environment.as_ref()) {
        command.env("HOME", home);
    }
    // Spawn failures never create a supervised child: map to InvalidRequest so
    // one-shot callers can surface a model-visible Failure instead of OutcomeUnknown.
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            backend_error(
                ExecutionBackendErrorKind::InvalidRequest,
                "literal process program was not found on the host PATH",
            )
        } else {
            backend_error(
                ExecutionBackendErrorKind::InvalidRequest,
                "host literal process spawn failed",
            )
        }
    })?;
    let raw_pid = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io_error("host literal process returned an invalid pid"));
    let raw_pid = match raw_pid {
        Ok(raw_pid) => raw_pid,
        Err(error) => {
            return Err(
                terminate_direct_child(&mut child, limits.cleanup_duration())
                    .err()
                    .unwrap_or(error),
            );
        }
    };
    let Some(stdin) = child.stdin.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            raw_pid,
            limits.cleanup_duration(),
            io_error("host literal process stdin was unavailable"),
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            raw_pid,
            limits.cleanup_duration(),
            io_error("host literal process stdout was unavailable"),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            raw_pid,
            limits.cleanup_duration(),
            io_error("host literal process stderr was unavailable"),
        ));
    };
    if let Err(error) = set_nonblocking(&stdin) {
        return Err(cleanup_setup_error(
            &mut child,
            raw_pid,
            limits.cleanup_duration(),
            error,
        ));
    }

    let output = Arc::new(Mutex::new(HostProcessOutput::default()));
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let output_io_failed = Arc::new(AtomicBool::new(false));
    let io_stop = Arc::new(AtomicBool::new(false));
    let stdout_thread = spawn_output_reader(
        stdout,
        HostProcessStream::Stdout,
        Arc::clone(&output),
        Arc::clone(&output_exceeded),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    );
    let stdout_thread = match stdout_thread {
        Ok(thread) => thread,
        Err(error) => {
            return Err(cleanup_setup_error(
                &mut child,
                raw_pid,
                limits.cleanup_duration(),
                error,
            ));
        }
    };
    let stderr_thread = spawn_output_reader(
        stderr,
        HostProcessStream::Stderr,
        Arc::clone(&output),
        Arc::clone(&output_exceeded),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    );
    let stderr_thread = match stderr_thread {
        Ok(thread) => thread,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            let cleanup =
                terminate_owned_process_group(&mut child, raw_pid, None, limits.cleanup_duration());
            let joined = stdout_thread.join();
            return Err(match (cleanup, joined) {
                (Err(cleanup), _) => cleanup,
                (Ok(_), Err(_)) => io_error("host process output worker failed during rollback"),
                (Ok(_), Ok(())) => error,
            });
        }
    };
    let stdin_bytes = Arc::clone(&request.request.stdin);
    let stdin_stop = Arc::clone(&io_stop);
    let stdin_thread = std::thread::Builder::new()
        .name(String::from("plak-process-stdin"))
        .spawn(move || {
            write_process_input(stdin, &stdin_bytes, &stdin_stop);
        })
        .map_err(|_| io_error("host process stdin worker could not start"));
    let stdin_thread = match stdin_thread {
        Ok(thread) => thread,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            let cleanup =
                terminate_owned_process_group(&mut child, raw_pid, None, limits.cleanup_duration());
            let stdout_joined = stdout_thread.join();
            let stderr_joined = stderr_thread.join();
            return Err(if let Err(cleanup) = cleanup {
                cleanup
            } else if stdout_joined.is_err() || stderr_joined.is_err() {
                io_error("host process output worker failed during rollback")
            } else {
                error
            });
        }
    };

    let mut terminal_status = None;
    let stop_reason = loop {
        if dropped.load(Ordering::Acquire) || control.cancellation().is_cancelled() {
            break HostProcessStop::Cancelled;
        }
        if output_exceeded.load(Ordering::Acquire) {
            break HostProcessStop::OutputLimit;
        }
        if output_io_failed.load(Ordering::Acquire) {
            break HostProcessStop::Io;
        }
        if started.elapsed() >= limits.execution_duration() {
            break HostProcessStop::TimedOut;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                terminal_status = Some(status);
                break HostProcessStop::Completed;
            }
            Ok(None) => std::thread::sleep(supervision_poll_interval(started.elapsed())),
            Err(_) => break HostProcessStop::Io,
        }
    };

    let cleanup_deadline = Instant::now() + limits.cleanup_duration();
    let cleanup_result =
        terminate_owned_process_group_until(&mut child, raw_pid, terminal_status, cleanup_deadline);
    if stop_reason == HostProcessStop::Completed && cleanup_result.is_ok() {
        // Preserve all bounded output from a normally completed owned group.
        // An escaped descendant may keep the pipes so this EOF drain
        // shares, rather than extends, the process cleanup deadline.
        wait_for_output_readers_until(&stdout_thread, &stderr_thread, cleanup_deadline);
    }
    io_stop.store(true, Ordering::Release);
    let stdin_joined = stdin_thread.join();
    let stdout_joined = stdout_thread.join();
    let stderr_joined = stderr_thread.join();
    terminal_status = Some(cleanup_result?);
    if stdin_joined.is_err() || stdout_joined.is_err() || stderr_joined.is_err() {
        return Err(io_error("host process I/O worker failed"));
    }

    if output_exceeded.load(Ordering::Acquire) || stop_reason == HostProcessStop::OutputLimit {
        // The child ran, and ran far enough to overflow the bound, before this
        // was noticed. Whatever it did, it did.
        return Err(
            resource_error("literal process output exceeds environment limits")
                .with_proven_no_effect(false),
        );
    }
    if output_io_failed.load(Ordering::Acquire) || stop_reason == HostProcessStop::Io {
        return Err(io_error("host process output or wait failed"));
    }
    match stop_reason {
        HostProcessStop::Cancelled => {
            return Err(backend_error(
                ExecutionBackendErrorKind::Cancelled,
                "host process group was cancelled and joined",
            ));
        }
        HostProcessStop::TimedOut => {
            return Err(backend_error(
                ExecutionBackendErrorKind::TimedOut,
                "host process execution timed out and its group was joined",
            ));
        }
        HostProcessStop::Completed => {}
        HostProcessStop::OutputLimit | HostProcessStop::Io => {
            return Err(io_error("host process stopped without a terminal result"));
        }
    }

    let status =
        terminal_status.ok_or_else(|| io_error("host process had no terminal exit status"))?;
    let collected = lock(&output);
    LiteralProcessResult::new(
        status.code(),
        collected.stdout.clone(),
        collected.stderr.clone(),
    )
}

fn spawn_output_reader<R>(
    reader: R,
    stream: HostProcessStream,
    output: Arc<Mutex<HostProcessOutput>>,
    exceeded: Arc<AtomicBool>,
    io_failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    limits: ExecutionEnvironmentLimits,
) -> Result<std::thread::JoinHandle<()>, ExecutionBackendError>
where
    R: AsFd + Read + Send + 'static,
{
    set_nonblocking(&reader)?;
    std::thread::Builder::new()
        .name(match stream {
            HostProcessStream::Stdout => String::from("plak-process-stdout"),
            HostProcessStream::Stderr => String::from("plak-process-stderr"),
        })
        .spawn(move || {
            read_bounded_process_output(
                reader, stream, &output, &exceeded, &io_failed, &stop, limits,
            );
        })
        .map_err(|_| io_error("host process output worker could not start"))
}

fn read_bounded_process_output<R: Read>(
    mut reader: R,
    stream: HostProcessStream,
    output: &Mutex<HostProcessOutput>,
    exceeded: &AtomicBool,
    io_failed: &AtomicBool,
    stop: &AtomicBool,
    limits: ExecutionEnvironmentLimits,
) {
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let read = match reader.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                io_failed.store(true, Ordering::Release);
                return;
            }
        };
        // A nonblocking pipe can remain perpetually readable when a detached
        // descendant writes continuously. Observe a stop that raced the
        // successful read before accounting or retaining that chunk.
        if stop.load(Ordering::Acquire) {
            return;
        }
        let mut collected = lock(output);
        let stdout_len = u64::try_from(collected.stdout.len()).unwrap_or(u64::MAX);
        let stderr_len = u64::try_from(collected.stderr.len()).unwrap_or(u64::MAX);
        let read_len = u64::try_from(read).unwrap_or(u64::MAX);
        let stream_would_exceed = match stream {
            HostProcessStream::Stdout => {
                stdout_len.saturating_add(read_len) > limits.max_stdout_bytes()
            }
            HostProcessStream::Stderr => {
                stderr_len.saturating_add(read_len) > limits.max_stderr_bytes()
            }
        };
        let combined_would_exceed = stdout_len
            .saturating_add(stderr_len)
            .saturating_add(read_len)
            > limits.max_combined_output_bytes();
        if stream_would_exceed || combined_would_exceed {
            exceeded.store(true, Ordering::Release);
            return;
        }
        match stream {
            HostProcessStream::Stdout => collected.stdout.extend_from_slice(&chunk[..read]),
            HostProcessStream::Stderr => collected.stderr.extend_from_slice(&chunk[..read]),
        }
    }
}

fn set_nonblocking<Fd: AsFd>(fd: &Fd) -> Result<(), ExecutionBackendError> {
    let flags = rustix::fs::fcntl_getfl(fd)
        .map_err(|_| io_error("host process pipe flags could not be read"))?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)
        .map_err(|_| io_error("host process pipe could not be made nonblocking"))
}

fn write_process_input(mut stdin: std::process::ChildStdin, bytes: &[u8], stop: &AtomicBool) {
    let mut written = 0_usize;
    let mut blocked_since = Instant::now();
    while written < bytes.len() && !stop.load(Ordering::Acquire) {
        match stdin.write(&bytes[written..]) {
            Ok(0) => return,
            Ok(count) => {
                written = written.saturating_add(count);
                blocked_since = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(supervision_poll_interval(blocked_since.elapsed()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn terminate_owned_process_group(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    mut status: Option<std::process::ExitStatus>,
    grace: Duration,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    let cleanup_deadline = Instant::now() + grace;
    terminate_owned_process_group_until(child, process_group, status.take(), cleanup_deadline)
}

fn terminate_owned_process_group_until(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    status: Option<std::process::ExitStatus>,
    cleanup_deadline: Instant,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    terminate_owned_process_group_with_grace(
        child,
        process_group,
        status,
        cleanup_deadline,
        Duration::from_secs(2),
    )
}

fn terminate_owned_process_group_with_grace(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    mut status: Option<std::process::ExitStatus>,
    cleanup_deadline: Instant,
    grace: Duration,
) -> Result<std::process::ExitStatus, ExecutionBackendError> {
    let mut group = GroupProbe::new(process_group);
    let now = Instant::now();
    let term_deadline = now + grace.min(cleanup_deadline.saturating_duration_since(now) / 2);
    let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::TERM);
    while Instant::now() < term_deadline {
        if status.is_none()
            && peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
        {
            status = child
                .try_wait()
                .map_err(|_| io_error("host process cleanup wait failed"))?;
        }
        if status.is_some() && group.members() == 0 {
            return status.ok_or_else(|| io_error("host process cleanup produced no status"));
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }

    if process_group_exists(process_group) {
        let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        group.checked = None;
    }
    while Instant::now() < cleanup_deadline {
        if status.is_none()
            && peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
        {
            status = child
                .try_wait()
                .map_err(|_| io_error("host process final join failed"))?;
        }
        if status.is_some() && group.members() == 0 {
            return status.ok_or_else(|| io_error("host process cleanup produced no status"));
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    Err(io_error(
        "host process group cleanup exceeded its configured bound",
    ))
}

fn terminate_owned_pty_group(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    status: Option<portable_pty::ExitStatus>,
    grace: Duration,
) -> Result<portable_pty::ExitStatus, ExecutionBackendError> {
    terminate_owned_pty_group_until(child, process_group, status, Instant::now() + grace)
}

fn terminate_owned_pty_group_until(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    status: Option<portable_pty::ExitStatus>,
    cleanup_deadline: Instant,
) -> Result<portable_pty::ExitStatus, ExecutionBackendError> {
    terminate_owned_pty_group_with_grace(
        child,
        process_group,
        status,
        cleanup_deadline,
        Duration::from_secs(2),
    )
}

fn terminate_owned_pty_group_with_grace(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    mut status: Option<portable_pty::ExitStatus>,
    cleanup_deadline: Instant,
    grace: Duration,
) -> Result<portable_pty::ExitStatus, ExecutionBackendError> {
    let mut group = GroupProbe::new(process_group);
    let now = Instant::now();
    let term_deadline = now + grace.min(cleanup_deadline.saturating_duration_since(now) / 2);
    let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::TERM);
    while Instant::now() < term_deadline {
        if status.is_none()
            && peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
        {
            status = child
                .try_wait()
                .map_err(|_| io_error("joined process PTY cleanup wait failed"))?;
        }
        if status.is_some() && group.members() == 0 {
            return status.ok_or_else(|| io_error("joined process PTY cleanup produced no status"));
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    if process_group_exists(process_group) {
        let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        group.checked = None;
    }
    while Instant::now() < cleanup_deadline {
        if status.is_none()
            && peek_leader_exit(process_group).is_ok_and(|exit| exit.is_some())
            && group.members() == 0
        {
            status = child
                .try_wait()
                .map_err(|_| io_error("joined process PTY final join failed"))?;
        }
        if status.is_some() && group.members() == 0 {
            return status.ok_or_else(|| io_error("joined process PTY cleanup produced no status"));
        }
        sleep_until_next_poll(now, cleanup_deadline);
    }
    Err(io_error(
        "joined process PTY group cleanup exceeded its configured bound",
    ))
}

fn cleanup_pty_setup_error(
    child: &mut dyn portable_pty::Child,
    process_group: rustix::process::Pid,
    grace: Duration,
    setup_error: ExecutionBackendError,
) -> ExecutionBackendError {
    terminate_owned_pty_group(child, process_group, None, grace)
        .err()
        .unwrap_or(setup_error)
}

fn wait_for_output_readers_until(
    stdout: &std::thread::JoinHandle<()>,
    stderr: &std::thread::JoinHandle<()>,
    deadline: Instant,
) {
    let started = Instant::now();
    while Instant::now() < deadline && (!stdout.is_finished() || !stderr.is_finished()) {
        sleep_until_next_poll(started, deadline);
    }
}

fn join_thread_until(thread: std::thread::JoinHandle<()>, deadline: Instant) -> bool {
    let started = Instant::now();
    while Instant::now() < deadline && !thread.is_finished() {
        sleep_until_next_poll(started, deadline);
    }
    thread.is_finished() && thread.join().is_ok()
}

fn cleanup_setup_error(
    child: &mut std::process::Child,
    process_group: rustix::process::Pid,
    grace: Duration,
    setup_error: ExecutionBackendError,
) -> ExecutionBackendError {
    terminate_owned_process_group(child, process_group, None, grace)
        .err()
        .unwrap_or(setup_error)
}

fn terminate_direct_child(
    child: &mut std::process::Child,
    grace: Duration,
) -> Result<(), ExecutionBackendError> {
    let _ = child.kill();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child
            .try_wait()
            .map_err(|_| io_error("host process rollback wait failed"))?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(io_error(
        "host process rollback exceeded its configured bound",
    ))
}

fn terminate_direct_pty_child(
    child: &mut dyn portable_pty::Child,
    grace: Duration,
) -> Result<(), ExecutionBackendError> {
    let _ = child.kill();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child
            .try_wait()
            .map_err(|_| io_error("joined process PTY rollback wait failed"))?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(io_error(
        "joined process PTY rollback exceeded its configured bound",
    ))
}

struct LeaderExit {
    code: Option<i32>,
}

fn peek_leader_exit(pid: rustix::process::Pid) -> rustix::io::Result<Option<LeaderExit>> {
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
    )
    .map(|status| {
        status.map(|status| LeaderExit {
            code: status.exit_status(),
        })
    })
}

struct GroupProbe {
    group: rustix::process::Pid,
    checked: Option<Instant>,
    count: usize,
}

impl GroupProbe {
    fn new(group: rustix::process::Pid) -> Self {
        Self {
            group,
            checked: None,
            count: 0,
        }
    }

    fn members(&mut self) -> usize {
        if !process_group_exists(self.group) {
            return 0;
        }
        if self
            .checked
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(250))
        {
            self.count = live_group_members(self.group);
            self.checked = Some(Instant::now());
        }
        self.count
    }
}

fn observe_leftover_group(
    group: &mut GroupProbe,
    code: Option<i32>,
    shared: &Mutex<JoinedSessionShared>,
) -> bool {
    if !process_group_exists(group.group) {
        return false;
    }
    let count = group.members();
    if count == 0 {
        return false;
    }
    let mut shared = lock(shared);
    let changed = shared.leftover.is_none();
    shared.leftover = Some(LeftoverJob {
        exit_code: code,
        processes: count,
    });
    if changed {
        shared
            .changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        if let Some(changes) = &shared.terminal_changes {
            changes.send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }
    true
}

// Zombies cannot run or receive signals. In particular, an orphaned zombie
// waiting for the host's init must not keep a server job alive forever.
fn live_group_members(group: rustix::process::Pid) -> usize {
    if !process_group_exists(group) {
        return 0;
    }
    live_group_members_impl(group).unwrap_or(1)
}

#[cfg(target_os = "linux")]
fn live_group_members_impl(group: rustix::process::Pid) -> Option<usize> {
    let mut count = 0;
    for entry in fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let (_, fields) = stat.rsplit_once(')')?;
        let mut fields = fields.split_whitespace();
        let state = fields.next()?;
        let _parent = fields.next()?;
        let pgid = fields.next()?.parse::<i32>().ok()?;
        if pgid == group.as_raw_pid() && state != "Z" && state != "X" {
            count += 1;
        }
    }
    Some(count)
}

#[cfg(target_os = "macos")]
fn live_group_members_impl(group: rustix::process::Pid) -> Option<usize> {
    let mut child = Command::new("/bin/ps")
        .args(["-axo", "pgid=,stat="])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut output = Vec::new();
    let collected = (|| {
        let mut stdout = child.stdout.take()?;
        set_nonblocking(&stdout).ok()?;
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut buffer = [0; 4096];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
            if output.len() > 1_048_576 || Instant::now() >= deadline {
                return None;
            }
        }
        loop {
            if let Some(status) = child.try_wait().ok()? {
                return status.success().then_some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    if collected.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    Some(
        String::from_utf8(output)
            .ok()?
            .lines()
            .filter(|line| {
                let mut fields = line.split_whitespace();
                fields.next().and_then(|pgid| pgid.parse::<i32>().ok()) == Some(group.as_raw_pid())
                    && fields.next().is_some_and(|state| !state.starts_with('Z'))
            })
            .count(),
    )
}

pub(super) fn process_group_exists(process_group: rustix::process::Pid) -> bool {
    match rustix::process::test_kill_process_group(process_group) {
        Ok(()) | Err(Errno::PERM) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod cleanup_contention_tests {
    use std::str::FromStr;

    use super::*;
    use crate::ExecutionEnvironmentScope;
    use plakous_model::RunId;

    type TestError = Box<dyn std::error::Error>;

    // This suite tests bounded mutex contention, not scheduler latency. Leave
    // enough test-only cleanup time for the process supervisor under a fully
    // parallel workspace run while retaining the independent two-second bound.
    const CLEANUP_DURATION: Duration = Duration::from_millis(250);

    #[test]
    fn unjoined_workers_are_recovery_required_not_terminal_failure() {
        assert_eq!(
            joined_process_final_status(JoinedProcessStatus::Failed, true, false, true),
            JoinedProcessStatus::CleanupUnresolved
        );
        assert!(!JoinedProcessStatus::CleanupUnresolved.is_terminal());
        assert_eq!(
            joined_process_final_status(JoinedProcessStatus::Failed, true, true, true),
            JoinedProcessStatus::Failed
        );
        assert!(JoinedProcessStatus::Failed.is_terminal());
    }

    fn sessions(value: u8) -> Result<(tempfile::TempDir, JoinedProcessSessions), TestError> {
        let workspace = tempfile::tempdir()?;
        let limits = ExecutionEnvironmentLimits::new(
            4096,
            4096,
            8192,
            4096,
            Duration::from_secs(30),
            CLEANUP_DURATION,
        )?;
        let run_id = RunId::from_str(&format!("00000000-0000-0000-0000-{value:012x}"))?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run_id),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::new(limits, [], [])?,
        )?;
        Ok((workspace, environment.joined_process_sessions()?))
    }

    fn sleeping_child(
        sessions: &JoinedProcessSessions,
    ) -> Result<JoinedProcessSessionId, TestError> {
        let shell = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|candidate| Path::new(candidate).is_file())
            .ok_or("test shell is unavailable")?;
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(shell, [String::from("-c"), String::from("sleep 30")])?,
            ".",
            [],
            false,
            JoinedProcessTerminalMode::Pipes,
        )?;
        Ok(sessions.start(
            request,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?)
    }

    fn assert_bounded(started: Instant) {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cleanup mutex admission exceeded its cleanup deadline"
        );
    }

    fn join_all_with_bounded_retries(sessions: &JoinedProcessSessions) -> Result<(), TestError> {
        for _ in 0..20 {
            match sessions.join_all() {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == ExecutionBackendErrorKind::CleanupUnresolved => {
                    assert!(!sessions.is_drained());
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err("process cleanup did not converge across bounded retries".into())
    }

    fn exiting_child(
        sessions: &JoinedProcessSessions,
        code: u8,
    ) -> Result<JoinedProcessSessionId, TestError> {
        let shell = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|candidate| Path::new(candidate).is_file())
            .ok_or("test shell is unavailable")?;
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(shell, [String::from("-c"), format!("exit {code}")])?,
            ".",
            [],
            true,
            JoinedProcessTerminalMode::Pipes,
        )?;
        Ok(sessions.start(
            request,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?)
    }

    /// A session that exited and was joined has a knowable disposition, and it
    /// keeps that disposition after it is retired. Nothing written to it can
    /// have had an effect, which is what makes a refusal definite rather than
    /// unknown.
    #[test]
    fn an_exited_session_reports_its_joined_status_while_active_and_after_retirement()
    -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(115)?;
        let id = exiting_child(&sessions, 7)?;
        let started = Instant::now();
        loop {
            let polled = sessions.poll(id, 4096, 16)?;
            if polled.status().is_terminal() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the child did not reach a joined terminal status"
            );
            std::thread::yield_now();
        }
        // Terminal but not retired: the session is still active and its handle
        // still resolves.
        assert_eq!(sessions.active_session_ids(), vec![id]);
        assert_eq!(
            sessions.joined_terminal_status(id),
            Some(JoinedProcessStatus::Exited(Some(7)))
        );
        assert_eq!(
            sessions
                .write_stdin(id, b"x", JoinedProcessInputDisposition::KeepOpen)
                .err()
                .map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::Conflict)
        );

        sessions.terminate(id)?;
        assert!(sessions.is_drained());
        // The tombstone keeps the status, so a late write is still knowable.
        assert_eq!(
            sessions.joined_terminal_status(id),
            Some(JoinedProcessStatus::Exited(Some(7)))
        );
        Ok(())
    }

    #[test]
    fn promoted_short_job_is_retired_without_observer_polling() -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(94)?;
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal("/bin/sh", ["-c", "cat; exit 7"].map(String::from))?,
            ".",
            [],
            true,
            JoinedProcessTerminalMode::Pipes,
        )?;
        let id = sessions.start(
            request,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.promote(id)?;
        sessions.write_stdin(id, b"", JoinedProcessInputDisposition::Close)?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let retired = {
                let state = lock(&sessions.inner.state);
                !state.active.contains_key(&id) && state.finished.contains_key(&id)
            };
            if retired {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "unobserved job was not retired promptly"
            );
            std::thread::yield_now();
        }
        assert_eq!(sessions.jobs()[0].status, crate::JobStatus::Exited(Some(7)));
        Ok(())
    }

    #[test]
    fn promoted_monitor_exits_on_owner_drop_without_terminal_publication() -> Result<(), TestError>
    {
        let (_workspace, sessions) = sessions(95)?;
        let id = sleeping_child(&sessions)?;
        let session = sessions.session(id)?;
        // Finish the real supervisor, then model one that vanished without publishing.
        {
            let mut owner = lock(&session.owner);
            let supervisor = owner.as_mut().ok_or("missing owner")?;
            supervisor.stop.store(true, Ordering::Release);
            assert!(supervisor.join_until(Instant::now() + Duration::from_secs(5))?);
            owner.take();
        }
        lock(&session.shared).status = JoinedProcessStatus::Running;
        let shared = Arc::clone(&session.shared);
        sessions.promote(id)?;
        drop(session);
        drop(sessions);
        let deadline = Instant::now() + Duration::from_secs(1);
        while Arc::strong_count(&shared) != 1 {
            assert!(
                Instant::now() < deadline,
                "job monitor retained the orphaned status buffer"
            );
            std::thread::yield_now();
        }
        Ok(())
    }

    #[tokio::test]
    async fn process_changes_wake_on_output_and_exit_without_promotion() -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(93)?;
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                "/bin/sh",
                [
                    "-c".to_owned(),
                    "read first; printf done; read second".to_owned(),
                ],
            )?,
            ".",
            [],
            true,
            JoinedProcessTerminalMode::Pipes,
        )?;
        let id = sessions.start(
            request,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        let mut changes = sessions.session_changes(id)?;
        sessions.write_stdin(id, b"go\n", JoinedProcessInputDisposition::KeepOpen)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                changes.borrow_and_update();
                let poll = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 64, 8)?;
                if !poll.records().is_empty() {
                    assert_eq!(poll.status(), JoinedProcessStatus::Running);
                    break;
                }
                changes.changed().await?;
            }
            sessions.write_stdin(id, b"finish\n", JoinedProcessInputDisposition::KeepOpen)?;
            loop {
                changes.borrow_and_update();
                let poll = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 64, 8)?;
                if poll.status().is_terminal() {
                    assert_eq!(poll.status(), JoinedProcessStatus::Exited(Some(0)));
                    assert_eq!(
                        poll.records()
                            .iter()
                            .flat_map(|record| record.bytes().iter())
                            .copied()
                            .collect::<Vec<_>>(),
                        b"done"
                    );
                    break;
                }
                changes.changed().await?;
            }
            Ok::<_, TestError>(())
        })
        .await??;
        sessions.join(id)?;
        Ok(())
    }

    fn await_terminal(sessions: &JoinedProcessSessions, id: JoinedProcessSessionId) {
        let started = Instant::now();
        while sessions.joined_terminal_status(id).is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "a child did not reach a joined terminal status"
            );
            std::thread::yield_now();
        }
    }

    /// A sequential program that starts one command at a time never exhausts
    /// the session limit, however many commands it runs.
    ///
    /// The limit bounds concurrent children. A live Code Mode cell looped over
    /// 22 files with one `cat` at a time and its 67th start was refused,
    /// because every finished session still counted against the limit.
    #[test]
    fn a_sequential_program_never_exhausts_the_session_limit() -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(117)?;
        // Comfortably past the limit, and none of these is ever explicitly
        // retired: this is a model that reads one page and moves on.
        for _ in 0..(MAX_JOINED_PROCESS_SESSIONS + 8) {
            let id = exiting_child(&sessions, 0)?;
            await_terminal(&sessions, id);
        }
        // Finished sessions are remembered so their output stays readable, but
        // only `MAX_JOINED_PROCESS_SESSIONS` of them, plus whichever session
        // the last admission added.
        assert!(
            sessions.active_session_ids().len() <= MAX_JOINED_PROCESS_SESSIONS + 1,
            "finished sessions are retained without bound"
        );
        join_all_with_bounded_retries(&sessions)?;
        Ok(())
    }

    /// The limit still bounds children that are actually running.
    #[test]
    fn running_children_still_count_against_the_session_limit() -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(118)?;
        for _ in 0..MAX_JOINED_PROCESS_SESSIONS {
            sleeping_child(&sessions)?;
        }
        let shell = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|candidate| Path::new(candidate).is_file())
            .ok_or("test shell is unavailable")?;
        let refused = sessions
            .start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::literal(
                        shell,
                        [String::from("-c"), String::from("sleep 30")],
                    )?,
                    ".",
                    [],
                    false,
                    JoinedProcessTerminalMode::Pipes,
                )?,
                ExecutionOperationControl::new(CancellationToken::new()),
            )
            .err()
            .ok_or("the session limit admitted one child too many")?;
        assert_eq!(refused.kind(), ExecutionBackendErrorKind::ResourceLimit);
        // The refusal happened before any spawn, which is what lets the model
        // see it as an ordinary tool failure.
        assert!(refused.kind().proven_no_effect());
        join_all_with_bounded_retries(&sessions)?;
        Ok(())
    }

    /// Reclaiming finished sessions never takes away output a model can still
    /// read: polling a finished session is how its last page is drained.
    #[test]
    fn a_finished_session_keeps_its_output_readable_across_later_starts() -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(119)?;
        let shell = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|candidate| Path::new(candidate).is_file())
            .ok_or("test shell is unavailable")?;
        let talker = sessions.start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal(
                    shell,
                    [String::from("-c"), String::from("printf hello; exit 3")],
                )?,
                ".",
                [],
                true,
                JoinedProcessTerminalMode::Pipes,
            )?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        await_terminal(&sessions, talker);
        for _ in 0..8 {
            let id = exiting_child(&sessions, 0)?;
            await_terminal(&sessions, id);
        }
        let polled = sessions.poll(talker, 4096, 16)?;
        assert_eq!(polled.status(), JoinedProcessStatus::Exited(Some(3)));
        let text = polled
            .records()
            .iter()
            .flat_map(|record| record.bytes().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(text, b"hello");
        join_all_with_bounded_retries(&sessions)?;
        Ok(())
    }

    /// A running session has no joined terminal status, so nothing about it is
    /// proven and the unknown-outcome path is untouched.
    #[test]
    fn a_running_session_has_no_joined_terminal_status() -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(116)?;
        let id = sleeping_child(&sessions)?;
        assert_eq!(sessions.joined_terminal_status(id), None);
        join_all_with_bounded_retries(&sessions)?;
        Ok(())
    }

    #[test]
    fn terminal_status_without_joined_owner_is_not_durable_proof() -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(120)?;
        let id = sleeping_child(&sessions)?;
        let session = sessions.session(id)?;
        // Model the narrow supervisor interval after publishing status and
        // before returning from its thread. The still-running owner must not
        // be certified merely because status has become terminal.
        lock(&session.shared).status = JoinedProcessStatus::Exited(Some(0));
        let started = Instant::now();
        assert!(!sessions.joined_terminal_identities().contains(&id.0));
        assert!(
            started.elapsed() < CLEANUP_DURATION / 2,
            "proof probe waited for an unfinished supervisor"
        );
        lock(&session.shared).status = JoinedProcessStatus::Running;
        join_all_with_bounded_retries(&sessions)?;
        Ok(())
    }

    #[test]
    fn panicked_supervisor_never_becomes_joined_proof_on_a_second_probe() {
        let mut owner = HostProcessOwner {
            stop: Arc::new(AtomicBool::new(false)),
            thread: Some(std::thread::spawn(|| {
                panic!("synthetic supervisor failure")
            })),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while owner
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert!(
            owner
                .thread
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
        );
        for _ in 0..2 {
            assert!(owner.join_until(Instant::now()).is_err());
        }
    }

    #[test]
    fn contended_operation_does_not_stall_joined_proof_probe() -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(121)?;
        let id = sleeping_child(&sessions)?;
        let session = sessions.session(id)?;
        let operation = session.enter()?;
        let probe = sessions.clone();
        let (send, receive) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            let _ = send.send(probe.joined_terminal_identities());
        });
        let result = receive.recv_timeout(CLEANUP_DURATION / 2);
        drop(operation);
        worker.join().map_err(|_| "joined proof probe panicked")?;
        assert!(
            !result
                .map_err(|_| "joined proof probe blocked on the operation lock")?
                .contains(&id.0),
            "an active child has no joined proof"
        );
        join_all_with_bounded_retries(&sessions)?;
        Ok(())
    }

    #[test]
    fn terminate_contention_is_bounded_and_preserves_correlated_owner() -> Result<(), TestError> {
        for value in 91..115 {
            let (_workspace, sessions) = sessions(value)?;
            let id = sleeping_child(&sessions)?;
            let identity_before = sessions.active_session_ids();
            let session = sessions.session(id)?;
            let operation = session.enter()?;

            let started = Instant::now();
            let error = sessions
                .terminate(id)
                .err()
                .ok_or("cleanup unexpectedly joined")?;
            assert_bounded(started);
            assert_eq!(error.kind(), ExecutionBackendErrorKind::CleanupUnresolved);
            assert_eq!(sessions.active_session_ids(), identity_before);
            assert!(!sessions.is_drained());

            drop(operation);
            join_all_with_bounded_retries(&sessions)?;
            assert!(sessions.is_drained());
        }
        Ok(())
    }

    #[test]
    fn join_all_handles_free_children_and_retains_contended_child_for_retry()
    -> Result<(), TestError> {
        let (_workspace, sessions) = sessions(92)?;
        let free = sleeping_child(&sessions)?;
        let contended = sleeping_child(&sessions)?;
        let session = sessions.session(contended)?;
        let operation = session.enter()?;

        let started = Instant::now();
        let error = sessions
            .join_all()
            .err()
            .ok_or("cleanup unexpectedly joined")?;
        assert_bounded(started);
        assert_eq!(error.kind(), ExecutionBackendErrorKind::CleanupUnresolved);
        assert!(sessions.session(free).is_err());
        assert!(sessions.session(contended).is_ok());
        assert_eq!(sessions.active_session_ids(), vec![contended]);

        drop(operation);
        join_all_with_bounded_retries(&sessions)?;
        assert!(sessions.is_drained());
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "process policy regression assertions"
)]
mod process_policy_tests {
    use super::*;

    #[test]
    fn direct_pty_uses_revalidated_alias_as_literal_program_name() {
        let temporary = tempfile::tempdir().expect("workspace");
        let startup = temporary.path().join("startup");
        fs::write(&startup, "printf unsafe; exit 99").expect("startup trap");
        // The direct host sees the enrolled alias; shell metacharacters in its
        // path are literal executable-name data, never launcher code.
        let alias = temporary.path().join("not mounted; $(exit 98)/sh");
        let program = Path::new("/bin/bash").canonicalize().expect("bash");
        fs::create_dir(alias.parent().expect("alias parent")).expect("alias directory");
        std::os::unix::fs::symlink(&program, &alias).expect("shell alias");
        let builder = joined_pty_command(
            &program,
            Some(&alias),
            [
                "-c",
                "case :$SHELLOPTS: in *:posix:*) printf '%s' \"$0\";; *) exit 97;; esac",
            ],
        );
        let argv = builder.get_argv();
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .env("BASH_ENV", &startup)
            .env("ENV", &startup)
            .output()
            .expect("exec launcher");
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            output.stdout,
            alias.to_str().expect("alias text").as_bytes()
        );
        assert_eq!(output.stderr, [] as [u8; 0]);
    }

    #[test]
    fn prepared_path_uses_snapshot_and_nonempty_delegation_precedence() {
        let process = HostProcessContext {
            fallback_path: "/captured/bin".into(),
            home: Some("/captured/home".into()),
            prefix: None,
        };
        let name = EnvironmentVariableName::new("PATH").expect("PATH");
        assert_eq!(
            host_process_path(&process, &[]).expect("fallback"),
            "/captured/bin"
        );
        assert_eq!(
            host_process_path(
                &process,
                &[(name.clone(), EnvironmentVariableValue("".into()))]
            )
            .expect("empty delegation"),
            "/captured/bin"
        );
        assert_eq!(
            host_process_path(
                &process,
                &[(name, EnvironmentVariableValue("/delegated/bin".into()))]
            )
            .expect("delegation"),
            "/delegated/bin"
        );
        assert_eq!(
            restore_path_after_login(&process, "printf plain").expect("no prefix"),
            "printf plain"
        );
    }

    #[test]
    fn prepared_home_prefers_a_curated_value_over_the_captured_host_home() {
        let process = HostProcessContext {
            fallback_path: "/captured/bin".into(),
            home: Some("/captured/home".into()),
            prefix: None,
        };
        let name = EnvironmentVariableName::new("HOME").expect("HOME");
        assert_eq!(
            host_process_home(&process, &[]),
            Some(Path::new("/captured/home"))
        );
        assert_eq!(
            host_process_home(
                &process,
                &[(name.clone(), EnvironmentVariableValue("".into()))]
            ),
            Some(Path::new("/captured/home"))
        );
        assert_eq!(
            host_process_home(
                &process,
                &[(name, EnvironmentVariableValue("/delegated/home".into()))]
            ),
            None
        );
        let homeless = HostProcessContext {
            fallback_path: "/captured/bin".into(),
            home: None,
            prefix: None,
        };
        assert_eq!(host_process_home(&homeless, &[]), None);
    }

    #[test]
    fn program_lookup_skips_a_non_executable_file_earlier_on_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");
        let shadow = root.path().join("shadow");
        let real = root.path().join("real");
        fs::create_dir(&shadow).expect("shadow dir");
        fs::create_dir(&real).expect("real dir");
        fs::write(shadow.join("tool"), "# sourced, not run\n").expect("shadow file");
        fs::set_permissions(shadow.join("tool"), fs::Permissions::from_mode(0o644))
            .expect("shadow mode");
        fs::write(real.join("tool"), "#!/bin/sh\n").expect("real file");
        fs::set_permissions(real.join("tool"), fs::Permissions::from_mode(0o755))
            .expect("real mode");
        let process = HostProcessContext {
            fallback_path: format!("{}:{}", shadow.display(), real.display()),
            home: None,
            prefix: None,
        };
        assert_eq!(
            resolve_host_process_program("tool", &process, &[]).expect("resolved"),
            real.join("tool")
        );
        let only_shadow = HostProcessContext {
            fallback_path: shadow.display().to_string(),
            home: None,
            prefix: None,
        };
        assert!(resolve_host_process_program("tool", &only_shadow, &[]).is_err());
    }
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

#[cfg(test)]
mod job_tests {
    use super::*;
    use crate::{ExecutionEnvironmentConfig, ExecutionEnvironmentScope, JobStatus, JobStopReason};
    use plakous_model::{CancellationToken, RunId};
    use std::str::FromStr;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn sessions(root: &Path) -> TestResult<JoinedProcessSessions> {
        Ok(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000691",
            )?),
            root,
            ".",
            ExecutionEnvironmentConfig::default(),
        )?
        .joined_process_sessions()?)
    }

    fn request(command: &str, stdin: bool) -> TestResult<JoinedProcessStartRequest> {
        Ok(JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(
                super::super::JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()])?,
                command,
            )?,
            ".",
            Vec::new(),
            stdin,
            JoinedProcessTerminalMode::Pipes,
        )?)
    }

    #[test]
    fn review_release_records_session_exit_stopper() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let id = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.promote(id)?;
        sessions.join_all()?;
        assert_eq!(
            sessions.jobs()[0].status,
            JobStatus::Stopped(JobStopReason::SessionExit)
        );
        Ok(())
    }

    #[test]
    fn review_cancel_between_observation_and_promotion_refuses_handle() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let cancellation = CancellationToken::new();
        let id = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(cancellation.clone()),
        )?;
        assert_eq!(
            sessions
                .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?
                .status(),
            JoinedProcessStatus::Running
        );
        cancellation.cancel();
        let error = sessions
            .promote(id)
            .err()
            .ok_or("cancelled foreground was promoted")?;
        assert_eq!(error.kind(), ExecutionBackendErrorKind::Cancelled);
        assert!(!sessions.is_promoted(id));
        sessions.join_all()?;
        Ok(())
    }

    #[test]
    fn review_running_snapshot_counts_toward_cap_and_is_consumable() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let source = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.promote(source)?;
        let mut views = sessions.jobs();
        let template = views[0].clone();
        for number in 2..=128 {
            let mut view = template.clone();
            view.number = number;
            views.push(view);
        }
        let restored = self::sessions(directory.path())?;
        restored.seed_job_counter(129);
        restored.restore_job_terminals(&views);
        assert_eq!(restored.jobs().len(), 128);
        assert_eq!(restored.jobs()[0].status, JobStatus::OutcomeUnknown);
        let id = restored
            .session_for_number(1)
            .ok_or("running snapshot missing")?;
        assert_eq!(
            restored
                .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?
                .status(),
            JoinedProcessStatus::OutcomeUnknown
        );
        assert!(
            restored
                .write_stdin(
                    id,
                    b"no live input",
                    JoinedProcessInputDisposition::KeepOpen
                )
                .is_err()
        );
        assert_eq!(
            restored.stop(id, JobStopReason::Model)?,
            JoinedProcessStatus::OutcomeUnknown
        );
        let next = restored.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        assert!(restored.promote(next).is_err());
        restored.consume_jobs(&[1]);
        assert!(restored.session_for_number(1).is_none());
        restored.promote(next)?;
        restored.join_all()?;
        sessions.join_all()?;
        Ok(())
    }

    #[test]
    fn review_consumption_removes_active_terminal_and_missing_stopper_is_unknown() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let id = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        // Hold the operation so the monitor cannot retire this terminal first.
        let session = sessions.session(id)?;
        let operation = session.enter()?;
        sessions.promote(id)?;
        lock(&session.shared).status = JoinedProcessStatus::Terminated;
        assert_eq!(sessions.jobs()[0].status, JobStatus::OutcomeUnknown);
        sessions.consume_jobs(&[1]);
        assert_eq!(sessions.jobs().len(), 1, "unjoined owner must remain owned");
        assert!(sessions.session_for_number(1).is_some());
        drop(operation);
        lock(&session.owner)
            .as_mut()
            .ok_or("owner")?
            .stop
            .store(true, Ordering::Release);
        session.join_finished_owner(JoinedProcessStatus::Terminated)?;
        Ok(())
    }

    /// A stopped job whose notice was consumed before its owner joined stays
    /// listed with its terminal status, but it is no longer unreported.
    #[test]
    fn a_consumed_terminal_job_awaiting_its_join_is_not_unreported() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let id = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        let session = sessions.session(id)?;
        let operation = session.enter()?;
        sessions.promote(id)?;
        assert!(!sessions.has_unreported_terminal_job(), "a running job");
        lock(&session.shared).status = JoinedProcessStatus::Terminated;
        assert!(sessions.has_unreported_terminal_job());
        sessions.consume_jobs(&[1]);
        assert_eq!(sessions.jobs().len(), 1, "unjoined owner must remain owned");
        assert_ne!(sessions.jobs()[0].status, JobStatus::Running);
        assert!(!sessions.has_unreported_terminal_job());
        drop(operation);
        lock(&session.owner)
            .as_mut()
            .ok_or("owner")?
            .stop
            .store(true, Ordering::Release);
        session.join_finished_owner(JoinedProcessStatus::Terminated)?;
        Ok(())
    }

    #[test]
    fn consume_running_job_keeps_the_owner_joinable() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let id = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.promote(id)?;
        sessions.consume_jobs(&[1]);
        assert!(sessions.session_for_number(1).is_some());
        sessions.stop(id, JobStopReason::Model)?;
        assert!(sessions.joined_terminal_status(id).is_some());
        Ok(())
    }

    #[test]
    fn tty_job_tail_normalizes_line_endings_and_all_controls() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let mut start = request("read line; printf 'one\\n two\\177\\302\\205\\n'", true)?;
        start.terminal_mode = JoinedProcessTerminalMode::UnixPty;
        let id = sessions.start(
            start,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.promote(id)?;
        sessions.write_stdin(id, b"done\n", JoinedProcessInputDisposition::KeepOpen)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while sessions.joined_terminal_status(id).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let output = sessions.jobs()[0].output.clone();
        sessions.join_all()?;
        assert!(output.ends_with("one\n two\u{fffd}\u{fffd}"), "{output:?}");
        assert!(!output.contains('\r'));
        Ok(())
    }

    #[test]
    fn review_job_tail_sanitizes_controls_and_is_bounded() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let id = sessions.start(
            request("read line; printf 'a\\000b\\001c\\r\\tend\\n'", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.promote(id)?;
        sessions.write_stdin(id, b"done\n", JoinedProcessInputDisposition::KeepOpen)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while sessions.joined_terminal_status(id).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(sessions.jobs()[0].output, "a\u{fffd}b\u{fffd}c\tend");
        sessions.join_all()?;
        Ok(())
    }

    #[test]
    fn promotion_detaches_cancel_and_per_id_join_retains_terminal_snapshot() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        sessions.seed_job_counter(7);
        let cancellation = CancellationToken::new();
        let id = sessions.start(
            request("read line; printf 'tail'; exit 7", true)?,
            ExecutionOperationControl::new(cancellation.clone()),
        )?;
        assert_eq!(JoinedProcessSessions::job_number(id), 7);
        sessions.promote(id)?;
        cancellation.cancel();
        sessions.write_stdin(id, b"finish\n", JoinedProcessInputDisposition::KeepOpen)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while sessions.joined_terminal_status(id).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(sessions.join(id)?, JoinedProcessStatus::Exited(Some(7)));
        assert!(!sessions.active_session_ids().contains(&id));
        let snapshot = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?;
        assert_eq!(snapshot.status(), JoinedProcessStatus::Exited(Some(7)));
        assert_eq!(
            snapshot
                .records()
                .iter()
                .flat_map(|record| record.bytes().iter())
                .copied()
                .collect::<Vec<_>>(),
            b"tail"
        );
        assert_eq!(sessions.jobs()[0].status, JobStatus::Exited(Some(7)));
        assert_eq!(sessions.jobs()[0].output, "tail");
        assert_eq!(sessions.next_job_number(), 8);
        sessions.consume_jobs(&[7]);
        assert_eq!(sessions.jobs(), [] as [crate::jobs::JobView; 0]);
        assert!(
            sessions
                .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn foreground_retention_does_not_count_as_unconsumed_job_rows() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        for promoted in [true, false] {
            for _ in 0..64 {
                let id = sessions.start(
                    request(if promoted { "read line" } else { "true" }, promoted)?,
                    ExecutionOperationControl::new(CancellationToken::new()),
                )?;
                if promoted {
                    sessions.promote(id)?;
                    sessions.write_stdin(id, b"done\n", JoinedProcessInputDisposition::KeepOpen)?;
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while sessions.joined_terminal_status(id).is_none() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(2));
                }
                if promoted {
                    sessions.join(id)?;
                }
            }
        }
        assert_eq!(sessions.jobs().len(), 64);
        let id = sessions.start(
            request("true", false)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.terminate(id)?;
        sessions.consume_jobs(&(1..=64).collect::<Vec<_>>());
        sessions.join_all()?;
        Ok(())
    }

    #[test]
    fn promotion_refuses_the_129th_unconsumed_row() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        for _ in 0..127 {
            let id = sessions.start(
                request("read line", true)?,
                ExecutionOperationControl::new(CancellationToken::new()),
            )?;
            sessions.promote(id)?;
            sessions.write_stdin(id, b"done\n", JoinedProcessInputDisposition::KeepOpen)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while sessions.joined_terminal_status(id).is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            sessions.join(id)?;
        }
        let first = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        let second = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.promote(first)?;
        let refused = sessions
            .promote(second)
            .err()
            .ok_or("129th promotion was admitted")?;
        assert!(!refused.proven_no_effect());
        assert_eq!(sessions.jobs().len(), 128);
        let foreground = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        assert!(sessions.promote(foreground).is_err());
        sessions.join_all()?;
        Ok(())
    }

    #[test]
    fn sixty_fifth_running_start_is_refused_before_spawn() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        for _ in 0..64 {
            sessions.start(
                request("read line", true)?,
                ExecutionOperationControl::new(CancellationToken::new()),
            )?;
        }
        let refused = sessions.start(
            request("printf bad > marker", false)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        );
        sessions.join_all()?;
        let error = refused.err().ok_or("65th start was admitted")?;
        assert!(error.to_string().contains("p64"), "{error}");
        assert!(!directory.path().join("marker").exists());
        Ok(())
    }

    #[test]
    fn host_capacity_refuses_the_129th_start_across_services_before_spawn() -> TestResult {
        let directory = tempfile::tempdir()?;
        let services = (0..3)
            .map(|_| sessions(directory.path()))
            .collect::<TestResult<Vec<_>>>()?;
        let capacity = JoinedProcessHostCapacity::default();
        for service in &services {
            service.bind_host_capacity(capacity.clone());
        }
        for service in &services[..2] {
            for _ in 0..64 {
                service.start(
                    request("read line", true)?,
                    ExecutionOperationControl::new(CancellationToken::new()),
                )?;
            }
        }
        let refused = services[2].start(
            request("printf bad > marker", false)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        );
        for service in &services {
            service.join_all()?;
        }
        assert!(refused.is_err());
        assert!(!directory.path().join("marker").exists());
        let id = services[2].start(
            request("true", false)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        services[2].terminate(id)?;
        Ok(())
    }

    #[test]
    fn explicit_stop_snapshot_keeps_attribution() -> TestResult {
        let directory = tempfile::tempdir()?;
        let sessions = sessions(directory.path())?;
        let id = sessions.start(
            request("read line", true)?,
            ExecutionOperationControl::new(CancellationToken::new()),
        )?;
        sessions.promote(id)?;
        sessions.stop_with_grace(id, JobStopReason::User, Duration::from_millis(200))?;
        assert_eq!(
            sessions.jobs()[0].status,
            JobStatus::Stopped(JobStopReason::User)
        );
        let restored = self::sessions(directory.path())?;
        restored.seed_job_counter(sessions.next_job_number());
        restored.restore_job_terminals(&sessions.jobs());
        restored.restore_job_terminals(&sessions.jobs());
        assert_eq!(restored.jobs().len(), 1);
        let restored_id = restored.session_for_number(1).ok_or("restored handle")?;
        assert_ne!(restored_id, id);
        assert_eq!(
            restored.stop(restored_id, JobStopReason::Model)?,
            JoinedProcessStatus::Terminated
        );
        assert_eq!(
            restored.jobs()[0].status,
            JobStatus::Stopped(JobStopReason::User)
        );
        restored.consume_jobs(&[1]);
        assert!(restored.session_for_number(1).is_none());
        let mut capped = sessions.jobs()[0].clone();
        capped.status = JobStatus::Stopped(JobStopReason::HardCap);
        let restored_cap = self::sessions(directory.path())?;
        restored_cap.seed_job_counter(sessions.next_job_number());
        restored_cap.restore_job_terminals(&[capped]);
        let cap_id = restored_cap.session_for_number(1).ok_or("restored cap")?;
        assert_eq!(
            restored_cap.stop(cap_id, JobStopReason::User)?,
            JoinedProcessStatus::TimedOut
        );
        assert_eq!(
            restored_cap.jobs()[0].status,
            JobStatus::Stopped(JobStopReason::HardCap)
        );
        Ok(())
    }
}

#[cfg(test)]
mod supervision_poll_tests {
    use super::{supervision_poll_interval, supervision_sleep};
    use std::time::Duration;

    #[test]
    fn bounded_waits_back_off_but_never_sleep_past_their_deadline() {
        let long = Duration::from_secs(5);
        assert_eq!(
            supervision_sleep(Duration::ZERO, long),
            Duration::from_millis(2)
        );
        assert_eq!(supervision_sleep(long, long), Duration::from_millis(32));
        assert_eq!(
            supervision_sleep(long, Duration::from_millis(5)),
            Duration::from_millis(5)
        );
        assert_eq!(supervision_sleep(long, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn long_lived_children_are_polled_slowly_and_young_ones_promptly() {
        assert_eq!(
            supervision_poll_interval(Duration::ZERO),
            Duration::from_millis(2)
        );
        assert_eq!(
            supervision_poll_interval(Duration::from_millis(500)),
            Duration::from_millis(8)
        );
        assert_eq!(
            supervision_poll_interval(Duration::from_secs(36_000)),
            Duration::from_millis(32)
        );
    }
}
