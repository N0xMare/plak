use super::{
    RunnerCursor, RunnerDimensions, RunnerEvent, RunnerId, RunnerProcessIdentity, RunnerRefusal,
    RunnerStatus, TerminalError, TerminalHandle, dto,
};

pub(super) fn handle(value: &dto::TerminalRef) -> TerminalHandle {
    TerminalHandle {
        identity: value.id.clone(),
        host_generation: value.host_generation,
        instance: value.instance.clone(),
    }
}

pub(super) fn reference(value: &TerminalHandle) -> dto::TerminalRef {
    dto::TerminalRef {
        id: value.identity.clone(),
        host_generation: value.host_generation,
        instance: value.instance.clone(),
    }
}

pub(super) fn cursor(value: RunnerCursor) -> dto::TerminalCursor {
    dto::TerminalCursor {
        instance: value.instance.as_str().into(),
        offset: value.offset,
    }
}

pub(super) fn status(value: RunnerStatus) -> dto::TerminalStatus {
    match value {
        RunnerStatus::Starting => dto::TerminalStatus::Starting,
        RunnerStatus::Running => dto::TerminalStatus::Running,
        RunnerStatus::Closing => dto::TerminalStatus::Closing,
        RunnerStatus::Exited => dto::TerminalStatus::Exited,
        RunnerStatus::TerminationUnknown => dto::TerminalStatus::TerminationUnknown,
        RunnerStatus::Lost => dto::TerminalStatus::Lost,
    }
}

pub(super) fn survivors(value: Vec<RunnerProcessIdentity>) -> Vec<dto::TerminalSurvivor> {
    value
        .into_iter()
        .map(|v| dto::TerminalSurvivor {
            pid: v.pid,
            started: v.started,
        })
        .collect()
}

pub(super) fn view(value: crate::terminal_registry::TerminalView) -> dto::TerminalView {
    let terminal = reference(&value.handle);
    let missing = dto::TerminalCursor {
        instance: terminal.instance.clone(),
        offset: 0,
    };
    let mut result = dto::TerminalView {
        terminal,
        session: value.key.session.as_str().into(),
        node: value.key.node.as_str().into(),
        terminal_id: value.key.terminal_id.as_str().into(),
        status: dto::TerminalStatus::Lost,
        oldest: missing.clone(),
        next: missing,
        input_next: 0,
        exit_code: None,
        survivors: Vec::new(),
    };
    if let Some(row) = value.row {
        result.status = if value.lost {
            dto::TerminalStatus::Lost
        } else {
            status(row.status)
        };
        result.oldest = cursor(row.oldest);
        result.next = cursor(row.next);
        result.input_next = row.input_next;
        result.exit_code = row.exit_code;
        result.survivors = survivors(row.survivors);
    }
    result
}

pub(super) fn dimensions(
    value: dto::TerminalDimensions,
) -> Result<RunnerDimensions, dto::ReadRefusal> {
    RunnerDimensions::new(value.rows, value.columns).map_err(|_| invalid())
}

pub(super) fn id(value: String) -> Result<RunnerId, dto::ReadRefusal> {
    value.try_into().map_err(|_| invalid())
}

pub(super) fn invalid() -> dto::ReadRefusal {
    super::super::validation("terminal", dto::ValidationCode::InvalidValue)
}

pub(super) fn refusal(value: TerminalError) -> dto::ReadRefusal {
    match value {
        TerminalError::Busy | TerminalError::Refused(RunnerRefusal::TerminalBusy) => {
            dto::ReadRefusal::TerminalBusy {}
        }
        TerminalError::Lost
        | TerminalError::Refused(RunnerRefusal::TerminalLost | RunnerRefusal::NamespaceExpired) => {
            dto::ReadRefusal::TerminalLost {}
        }
        TerminalError::Conflict => dto::ReadRefusal::Conflict {
            etag: String::new(),
        },
        TerminalError::Refused(RunnerRefusal::Backpressure | RunnerRefusal::Quota) => {
            dto::ReadRefusal::TerminalBackpressure {}
        }
        _ => dto::ReadRefusal::WorkspaceUnavailable {},
    }
}

pub(super) fn event(value: RunnerEvent) -> dto::TerminalEvent {
    match value {
        RunnerEvent::IdleWarning { remaining_ms } => {
            dto::TerminalEvent::IdleWarning { remaining_ms }
        }
        RunnerEvent::Data { offset, bytes_b64 } => dto::TerminalEvent::Data { offset, bytes_b64 },
        RunnerEvent::InputAccepted { next } => dto::TerminalEvent::InputAccepted { next },
        RunnerEvent::OutputGap { oldest } => dto::TerminalEvent::OutputGap {
            oldest: cursor(oldest),
        },
        RunnerEvent::Exit {
            survivors: escaped,
            status: state,
            code,
        } => dto::TerminalEvent::Exit {
            status: status(state),
            code,
            survivors: survivors(escaped),
        },
        RunnerEvent::Refused { refusal: reason } => dto::TerminalEvent::Refused {
            reason: refusal(TerminalError::Refused(reason)),
        },
    }
}
