use super::{
    Arc, JobOutputFile, JobOutputRoot, JobOutputs, JoinedProcessSessionId, JoinedProcessSessions,
    JoinedSession, JoinedSessionShared, Ordering, lock,
};

impl JoinedProcessSessions {
    /// Runs the retention sweep and makes room under the jobs root's budget
    /// for one promoted job's file; returns whether it fits. It walks the
    /// jobs root, so async callers run it on the blocking pool. It publishes
    /// nothing, so a caller dropped after it leaves no job behind. A
    /// promotion the service would refuse now walks nothing and fits nothing.
    pub(crate) fn reserve_job_output(&self, id: JoinedProcessSessionId) -> bool {
        if self.runner_arm().is_some() {
            return false;
        }
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

    /// Drops snapshots only after their terminal observation has committed.
    pub(crate) fn consume_jobs(&self, numbers: &[u32]) {
        if let Some(arm) = self.runner_arm() {
            arm.consume(numbers);
        }
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
}
