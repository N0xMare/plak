use super::{
    Applier, CallRead, CallTarget, FeedCallerLineage, FeedOperationKind, FeedOperationState,
    ProcessNotice, ProcessOutcome, ProcessTrack, ProjectError, TranscriptRow, completed_tool_state,
    host_owns_subject, operation_read, settle, tool_row,
};

impl Applier<'_> {
    pub(super) fn operation(
        &mut self,
        operation_id: u64,
        turn_id: Option<String>,
        kind: &FeedOperationKind,
        state: FeedOperationState,
    ) -> Result<(), ProjectError> {
        match kind {
            FeedOperationKind::Effect {
                call_id,
                exposed_name,
                lineage,
                subject,
            } => {
                let (parent_call_id, cell_id) = match lineage {
                    FeedCallerLineage::LocalProgrammatic {
                        parent_call_id,
                        cell_id,
                        ..
                    } => (
                        parent_call_id.map(|id| id.to_string()),
                        Some(cell_id.clone()),
                    ),
                    _ => (None, None),
                };
                self.effect(
                    operation_read(operation_id, kind, state),
                    turn_id,
                    &call_id.to_string(),
                    exposed_name.as_str(),
                    subject.as_deref(),
                    (parent_call_id, cell_id),
                    state,
                )
            }
            FeedOperationKind::CodeCell {
                call_id,
                exposed_name,
                subject,
            } => self.effect(
                operation_read(operation_id, kind, state),
                turn_id,
                &call_id.to_string(),
                exposed_name.as_str(),
                subject.as_deref(),
                (None, None),
                state,
            ),
            FeedOperationKind::ProcessSession { .. } => self.process(operation_id, state),
            FeedOperationKind::ProviderAttempt { .. } if state == FeedOperationState::Completed => {
                let anchor = self.seq();
                if let Some(turn) = turn_id
                    .as_ref()
                    .and_then(|turn| self.views.state.open_turns.get_mut(turn))
                {
                    turn.round_anchor = Some(anchor);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    #[allow(clippy::too_many_arguments, reason = "one fact's fields")]
    fn effect(
        &mut self,
        read: Option<CallTarget>,
        turn_id: Option<String>,
        call_id: &str,
        exposed: &str,
        subject: Option<&str>,
        lineage_parent_cell: (Option<String>, Option<String>),
        state: FeedOperationState,
    ) -> Result<(), ProjectError> {
        let nested = lineage_parent_cell.1.is_some();
        let (owning_parent, owning_cell) = lineage_parent_cell;
        let read = match read {
            Some(target) => self.call_read(target)?,
            None => CallRead::NotRetained,
        };
        let (canonical, arguments, result) = match read {
            CallRead::Tool {
                canonical,
                arguments,
                result,
                ..
            } => (canonical, arguments, result),
            CallRead::Nested { name, arguments } => (name, arguments, None),
            _ => {
                let canonical = self
                    .views
                    .state
                    .calls
                    .get(call_id)
                    .and_then(|call| self.views.rows.get(&call.key))
                    .and_then(TranscriptRow::as_tool)
                    .map_or_else(|| exposed.to_owned(), |tool| tool.name.clone());
                (canonical, None, None)
            }
        };
        let key = self.call_key(call_id, turn_id, || {
            tool_row(call_id, &canonical, exposed, arguments.as_deref())
        });
        self.count(call_id);
        let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&key).cloned() else {
            if let Some(call) = self.views.state.calls.get_mut(call_id) {
                call.operated = true;
                call.settled |= state != FeedOperationState::Dispatched
                    && (state != FeedOperationState::Completed || nested);
            }
            return Ok(());
        };
        tool.name.clone_from(&canonical);
        tool.kind = crate::activity::tool_activity_kind(&canonical);
        tool.exposed_name = (exposed != canonical).then(|| exposed.to_owned());
        if let Some(subject) = subject
            && !(tool.subject.is_some() && host_owns_subject(&canonical, exposed))
        {
            tool.subject = Some(subject.to_owned());
        }
        if owning_parent.is_some() {
            tool.parent_call_id = owning_parent;
        }
        if owning_cell.is_some() {
            tool.cell_id = owning_cell;
        }
        let committed_at = self.committed_at();
        let Some(call) = self.views.state.calls.get_mut(call_id) else {
            return Ok(());
        };
        call.operated = true;
        match state {
            FeedOperationState::Dispatched => {
                tool.started_at_ms.get_or_insert(committed_at);
            }
            FeedOperationState::Completed
            | FeedOperationState::OutcomeUnknown
            | FeedOperationState::ProvenNotDispatched => {
                if tool.started_at_ms.is_some() {
                    tool.finished_at_ms.get_or_insert(committed_at);
                }
                // A history call settles from its committed result; only a
                // nested call, which has none, settles on completion.
                let settles = state != FeedOperationState::Completed || nested;
                if !call.settled && settles {
                    call.settled = true;
                    match (state, result) {
                        (FeedOperationState::Completed, Some(result)) => settle(&mut tool, &result),
                        (FeedOperationState::Completed, None) => {
                            tool.state = completed_tool_state(&tool.name, Some((true, "")));
                        }
                        (FeedOperationState::ProvenNotDispatched, _) => {
                            "cancelled".clone_into(&mut tool.state);
                        }
                        _ => "interrupted".clone_into(&mut tool.state),
                    }
                }
            }
            _ => {}
        }
        let waited_ms = tool.duration_ms();
        if let Some(wait) = tool.wait.as_mut() {
            wait.waited_ms = waited_ms;
        }
        self.replace(key, TranscriptRow::Tool(tool));
        Ok(())
    }

    fn process(
        &mut self,
        operation_id: u64,
        state: FeedOperationState,
    ) -> Result<(), ProjectError> {
        let committed_at = self.committed_at();
        let track = self
            .views
            .state
            .processes
            .entry(operation_id)
            .or_insert(ProcessTrack {
                dispatched_at_ms: None,
                reported: false,
            });
        match state {
            FeedOperationState::Dispatched => {
                track.dispatched_at_ms.get_or_insert(committed_at);
                return Ok(());
            }
            FeedOperationState::Completed | FeedOperationState::OutcomeUnknown => {}
            _ => return Ok(()),
        }
        if track.reported {
            return Ok(());
        }
        let dispatched_at = track.dispatched_at_ms;
        let CallRead::Job {
            number: Some(number),
            command,
            end: Some(end),
        } = self.call_read(CallTarget::Operation(operation_id))?
        else {
            return Ok(());
        };
        if let Some(track) = self.views.state.processes.get_mut(&operation_id) {
            track.reported = true;
        }
        self.push(TranscriptRow::Process(ProcessNotice {
            job_id: format!("p{number}"),
            outcome: if end.stopped {
                ProcessOutcome::Stopped
            } else {
                ProcessOutcome::Exited {
                    code: end.exit_code,
                }
            },
            duration_ms: dispatched_at.and_then(|start| committed_at.checked_sub(start)),
            subject: command,
        }));
        Ok(())
    }
}
