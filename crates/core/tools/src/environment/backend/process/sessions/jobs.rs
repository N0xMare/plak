use super::{
    Arc, ExecutionBackendError, ExecutionBackendErrorKind, FOREGROUND, Instant, JobObserver,
    JobOutputAdmission, JoinedProcessServiceState, JoinedProcessSessionId, JoinedProcessSessions,
    JoinedProcessStatus, JoinedSession, NonZeroU64, Ordering, PROMOTED, Receiver, backend_error,
    cancel_foreground, cleanup_unresolved_error, io_error, lock, resource_error, sync_channel,
};

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
        if let Some(arm) = self.runner_arm() {
            return Ok(arm.session_changes(id));
        }
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

    /// Seeds the durable per-session number counter without ever moving it back.
    pub(crate) fn seed_job_counter(&self, next_id: u32) {
        if let Some(arm) = self.runner_arm() {
            arm.seed_job_numbers(next_id);
        }
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
        let local = u32::try_from(lock(&self.inner.state).next).unwrap_or(u32::MAX);
        match self.runner_arm() {
            Some(arm) => local.max(arm.next_job_number()),
            None => local,
        }
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
        if let Some(arm) = self.runner_arm() {
            return arm.is_promoted(id);
        }
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
        if let Some(arm) = self.runner_arm() {
            return arm.disown_leftovers();
        }
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
        if let Some(arm) = self.runner_arm()
            && arm.knows(id)
        {
            return true;
        }
        self.session(id).is_ok()
    }

    /// Promotes a process and detaches dispatch cancellation before producing its handle.
    /// It blocks on the jobs root's retention sweep and budget; async callers
    /// reserve on the blocking pool and promote with [`Self::promote_reserved`].
    ///
    /// # Errors
    /// Rejects a foreign session or an exhausted bounded notice ledger.
    pub(crate) fn promote(&self, id: JoinedProcessSessionId) -> Result<(), ExecutionBackendError> {
        if let Some(arm) = self.runner_arm() {
            return arm.promote(id);
        }
        let fits = self.reserve_job_output(id);
        self.promote_reserved(id, fits)
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
        if let Some(arm) = self.runner_arm() {
            return arm.promote(id);
        }
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

    /// Decides the read exception for `path`; `None` leaves it to containment.
    pub(in crate::environment::backend) fn admit_job_output(
        &self,
        path: &str,
    ) -> Option<JobOutputAdmission> {
        lock(&self.inner.outputs)
            .as_ref()
            .and_then(|outputs| outputs.admit(path))
    }

    /// Whether the service retains any promoted running or unconsumed job.
    #[must_use]
    pub fn has_jobs(&self) -> bool {
        if let Some(arm) = self.runner_arm()
            && !arm.jobs().is_empty()
        {
            return true;
        }
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
        let runner = self.runner_arm().map_or(0, |arm| arm.running_job_count());
        if runner != 0 {
            return runner;
        }
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
        if let Some(arm) = self.runner_arm()
            && arm.has_unreported_terminal_job()
        {
            return true;
        }
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
        let mut jobs: Vec<crate::JobView> = state
            .active
            .iter()
            .chain(&state.finished)
            .filter(|(_, session)| session.promoted.load(Ordering::Acquire) == PROMOTED)
            .map(|(id, session)| self.job_view(*id, session))
            .collect();
        drop(state);
        if let Some(arm) = self.runner_arm() {
            jobs.extend(arm.jobs());
        }
        jobs
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
            JoinedProcessStatus::Released => crate::JobStatus::Released,
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
        if let Some(arm) = self.runner_arm()
            && let Some(id) = arm.session_for_number(number)
        {
            return Some(id);
        }
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

    pub(crate) fn runtime_owner_generation(&self) -> NonZeroU64 {
        self.inner.owner_generation
    }
}
