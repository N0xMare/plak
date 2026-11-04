use super::{
    Duration, ExecutionBackendError, Instant, JoinedProcessSessionId, JoinedProcessSessions,
    JoinedProcessStatus, Ordering, PROMOTED, cleanup_unresolved_error, conflict_error,
    invalid_request, lock, recovery_required_error,
};

impl JoinedProcessSessions {
    /// Joins exactly one exited process without signalling another process.
    ///
    /// # Errors
    /// Rejects running/foreign sessions or a failed supervisor join.
    pub fn join(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        if let Some(arm) = self.arm_for(id) {
            return arm.join(id);
        }
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
        if let Some(arm) = self.arm_for(id) {
            let stopped = arm.stop_with_grace(id, by, grace);
            arm.report(self);
            return stopped;
        }
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
        if let Some(arm) = self.runner_arm() {
            let stopped = arm.stop_jobs(by, grace);
            arm.report(self);
            return stopped;
        }
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
        if let Some(arm) = self.runner_arm() {
            if *lock(&self.inner.agent) != owner {
                return Ok(());
            }
            let stopped = arm.stop_jobs(by, grace);
            arm.report(self);
            return stopped;
        }
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
        if let Some(arm) = self.runner_arm() {
            let stopped = arm.stop_foreground();
            arm.report(self);
            return stopped;
        }
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

    /// Terminates and joins one exact session, then retains a bounded tombstone.
    ///
    /// # Errors
    ///
    /// Rejects foreign/stale sessions or reports a failed supervisor join.
    pub fn terminate(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<JoinedProcessStatus, ExecutionBackendError> {
        if let Some(arm) = self.arm_for(id) {
            let stopped = arm.terminate(id);
            arm.report(self);
            return stopped;
        }
        let deadline = Instant::now() + self.inner.limits.cleanup_duration();
        let session = self.session(id)?;
        let _operation = session.enter_until(deadline)?;
        if !lock(&session.shared).status.is_terminal() {
            lock(&session.stopped_by).get_or_insert(crate::JobStopReason::Model);
        }
        lock(&session.input).take();
        let mut owner = lock(&session.owner);
        let joined = if let Some(owner) = owner.as_mut() {
            owner.request_stop();
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
        if let Some(arm) = self.runner_arm() {
            let stopped = arm.stop_foreground();
            arm.report(self);
            return stopped;
        }
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
            owner.request_stop();
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
}
