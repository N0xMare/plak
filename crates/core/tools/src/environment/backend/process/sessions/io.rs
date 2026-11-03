use super::{
    Arc, ExecutionBackendError, ExecutionBackendErrorKind, JOINED_STDIN_CLOSED,
    JoinedProcessCursorPollResult, JoinedProcessHostCapacity, JoinedProcessInputDisposition,
    JoinedProcessOutputCursor, JoinedProcessOutputRecord, JoinedProcessPollResult,
    JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessStartRequest, JoinedProcessStatus,
    MAX_JOINED_PROCESS_SESSION_POLL_RECORDS, MAX_JOINED_PROCESS_SESSION_WRITE_BYTES,
    MAX_LITERAL_PROCESS_STDIN_BYTES, OperationContext, Ordering, PendingJobOutput, backend_error,
    close_joined_session_input, conflict_error, invalid_request, lock, resource_error,
    start_joined_host_process, write_joined_session_input,
};

impl JoinedProcessSessions {
    pub(crate) fn projection_state(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<crate::internal::ProcessSessionProjectionState, ExecutionBackendError> {
        if let Some(arm) = self.runner_arm() {
            return Ok(arm.projection_state(id));
        }
        let session = self.session(id)?;
        Ok((
            Arc::clone(&session.consumer_cursor),
            Arc::clone(&session.consumer_pending_utf8),
            Arc::clone(&session.consumer_quiet_polls),
        ))
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
        control: OperationContext,
    ) -> Result<JoinedProcessSessionId, ExecutionBackendError> {
        if let Some(arm) = self.runner_arm() {
            return arm.start(request, control);
        }
        control.check_cancelled()?;
        let Some(process) = self.inner.process.clone() else {
            return Err(backend_error(
                ExecutionBackendErrorKind::Unsupported,
                "process sessions of a session on another runner are not carried yet",
            ));
        };
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
            &process,
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
        control: OperationContext,
        max_output_bytes: usize,
    ) -> Result<JoinedProcessSessionId, ExecutionBackendError> {
        request.output_spool_threshold = Some(max_output_bytes);
        self.start(request, control)
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
        if let Some(arm) = self.runner_arm() {
            return arm.write_stdin(id, bytes, disposition);
        }
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
        if let Some(arm) = self.runner_arm() {
            return arm.poll(id, max_bytes, max_records);
        }
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
        if let Some(arm) = self.runner_arm() {
            return arm.poll_from(id, cursor, max_bytes, max_records);
        }
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
        if let Some(arm) = self.runner_arm() {
            return arm.poll_tail(id, max_bytes, max_records);
        }
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
}
