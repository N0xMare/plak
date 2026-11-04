//! The JSON schema of the process-session verbs: start requests, polls,
//! stops, job views and statuses. Requests decode through the validating
//! constructors a local caller uses.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use plakous_model::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::{
    ExecutionBackendError, JoinedProcessCommand, JoinedProcessCursorPollResult,
    JoinedProcessOutputCursor, JoinedProcessOutputRecord, JoinedProcessOutputStream,
    JoinedProcessSessionId, JoinedProcessShellProfile, JoinedProcessStartRequest,
    JoinedProcessStatus, JoinedProcessTerminalMode,
};
use super::wire::{from_value, hex, malformed, path_text, to_value, unhex};
use crate::{JobStatus, JobStopReason, JobView};

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CommandWire {
    Literal {
        executable: String,
        arguments: Vec<String>,
    },
    /// The runner enrolls the shell itself, against its own filesystem.
    Shell {
        executable: String,
        arguments: Vec<String>,
        command: String,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StartWire {
    command: CommandWire,
    cwd: String,
    stdin: String,
    keep_stdin_open: bool,
    terminal: String,
    output_spool_threshold: Option<usize>,
    /// The lowest number the session may take: past every number the host
    /// has seen on the line, so a restarted runner never reuses one.
    next_number: u32,
}

pub(super) fn encode_start(
    request: &JoinedProcessStartRequest,
    next_number: u32,
) -> Result<Value, ExecutionBackendError> {
    let command = match &request.command {
        JoinedProcessCommand::Literal {
            executable,
            arguments,
        } => CommandWire::Literal {
            executable: path_text(executable)?,
            arguments: arguments.to_vec(),
        },
        JoinedProcessCommand::Shell { profile, command } => CommandWire::Shell {
            executable: path_text(profile.executable())?,
            arguments: profile.arguments().to_vec(),
            command: command.to_string(),
        },
    };
    to_value(&StartWire {
        command,
        cwd: path_text(&request.cwd)?,
        stdin: hex(&request.stdin),
        keep_stdin_open: request.keep_stdin_open,
        terminal: match request.terminal_mode {
            JoinedProcessTerminalMode::Pipes => "pipes",
            JoinedProcessTerminalMode::UnixPty => "unix_pty",
        }
        .to_owned(),
        output_spool_threshold: request.output_spool_threshold,
        next_number,
    })
}

/// A start request and the lowest number its session may take.
pub(super) fn decode_start(
    value: Value,
) -> Result<(JoinedProcessStartRequest, u32), ExecutionBackendError> {
    let wire: StartWire = from_value(value)?;
    let command = match wire.command {
        CommandWire::Literal {
            executable,
            arguments,
        } => JoinedProcessCommand::literal(executable, arguments)?,
        CommandWire::Shell {
            executable,
            arguments,
            command,
        } => JoinedProcessCommand::shell(
            JoinedProcessShellProfile::new(executable, arguments)?,
            command,
        )?,
    };
    let terminal = match wire.terminal.as_str() {
        "pipes" => JoinedProcessTerminalMode::Pipes,
        "unix_pty" => JoinedProcessTerminalMode::UnixPty,
        _ => return Err(malformed()),
    };
    let mut request = JoinedProcessStartRequest::new(
        command,
        PathBuf::from(wire.cwd),
        unhex(&wire.stdin)?,
        wire.keep_stdin_open,
        terminal,
    )?;
    request.output_spool_threshold = wire.output_spool_threshold;
    Ok((request, wire.next_number))
}

pub(super) fn encode_session(id: JoinedProcessSessionId) -> String {
    hex(&id.0)
}

pub(super) fn decode_session(text: &str) -> Result<JoinedProcessSessionId, ExecutionBackendError> {
    let bytes: [u8; 16] = unhex(text)?.try_into().map_err(|_| malformed())?;
    Ok(JoinedProcessSessionId(bytes))
}

/// One process session, as the runner names it.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionWire {
    pub(super) session: String,
}

/// A started session and the runner instance that holds it.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StartedWire {
    pub(super) session: String,
    pub(super) instance: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WriteWire {
    pub(super) session: String,
    pub(super) bytes: String,
    pub(super) close: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PollWire {
    pub(super) session: String,
    pub(super) cursor: u64,
    pub(super) max_bytes: usize,
    pub(super) max_records: usize,
    /// How long the runner may wait for output or an exit; a yield ends the
    /// wait early.
    pub(super) wait_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordWire {
    sequence: u64,
    cursor: u64,
    stream: String,
    bytes: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PolledWire {
    records: Vec<RecordWire>,
    status: Value,
    next_cursor: u64,
    more_output: bool,
    skipped_bytes: u64,
}

pub(super) fn encode_polled(
    result: &JoinedProcessCursorPollResult,
) -> Result<Value, ExecutionBackendError> {
    to_value(&PolledWire {
        records: result
            .records()
            .iter()
            .map(|record| RecordWire {
                sequence: record.sequence,
                cursor: record.cursor,
                stream: match record.stream {
                    JoinedProcessOutputStream::Stdout => "stdout",
                    JoinedProcessOutputStream::Stderr => "stderr",
                    JoinedProcessOutputStream::Terminal => "terminal",
                }
                .to_owned(),
                bytes: hex(&record.bytes),
            })
            .collect(),
        status: encode_status(result.status),
        next_cursor: result.next_cursor.0,
        more_output: result.more_output,
        skipped_bytes: result.skipped_bytes,
    })
}

pub(super) fn decode_polled(
    value: Value,
) -> Result<JoinedProcessCursorPollResult, ExecutionBackendError> {
    let wire: PolledWire = from_value(value)?;
    let records = wire
        .records
        .into_iter()
        .map(|record| {
            Ok(JoinedProcessOutputRecord {
                sequence: record.sequence,
                cursor: record.cursor,
                stream: match record.stream.as_str() {
                    "stdout" => JoinedProcessOutputStream::Stdout,
                    "stderr" => JoinedProcessOutputStream::Stderr,
                    "terminal" => JoinedProcessOutputStream::Terminal,
                    _ => return Err(malformed()),
                },
                bytes: Arc::from(unhex(&record.bytes)?),
            })
        })
        .collect::<Result<Vec<_>, ExecutionBackendError>>()?;
    Ok(JoinedProcessCursorPollResult {
        records: records.into(),
        status: decode_status(&wire.status)?,
        next_cursor: JoinedProcessOutputCursor(wire.next_cursor),
        more_output: wire.more_output,
        skipped_bytes: wire.skipped_bytes,
    })
}

pub(super) fn encode_status(status: JoinedProcessStatus) -> Value {
    let name = match status {
        JoinedProcessStatus::Running => "running",
        JoinedProcessStatus::Exited(code) => return json!({ "status": "exited", "code": code }),
        JoinedProcessStatus::OutcomeUnknown => "outcome_unknown",
        JoinedProcessStatus::Released => "released",
        JoinedProcessStatus::Terminated => "terminated",
        JoinedProcessStatus::Cancelled => "cancelled",
        JoinedProcessStatus::TimedOut => "timed_out",
        JoinedProcessStatus::OutputLimit => "output_limit",
        JoinedProcessStatus::Failed => "failed",
        JoinedProcessStatus::CleanupUnresolved => "cleanup_unresolved",
    };
    json!({ "status": name })
}

pub(super) fn decode_status(value: &Value) -> Result<JoinedProcessStatus, ExecutionBackendError> {
    Ok(match status_name(value)? {
        "running" => JoinedProcessStatus::Running,
        "exited" => JoinedProcessStatus::Exited(code(value)?),
        "outcome_unknown" => JoinedProcessStatus::OutcomeUnknown,
        "released" => JoinedProcessStatus::Released,
        "terminated" => JoinedProcessStatus::Terminated,
        "cancelled" => JoinedProcessStatus::Cancelled,
        "timed_out" => JoinedProcessStatus::TimedOut,
        "output_limit" => JoinedProcessStatus::OutputLimit,
        "failed" => JoinedProcessStatus::Failed,
        "cleanup_unresolved" => JoinedProcessStatus::CleanupUnresolved,
        _ => return Err(malformed()),
    })
}

fn status_name(value: &Value) -> Result<&str, ExecutionBackendError> {
    value
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(malformed)
}

fn code(value: &Value) -> Result<Option<i32>, ExecutionBackendError> {
    match value.get("code") {
        None | Some(Value::Null) => Ok(None),
        Some(code) => code
            .as_i64()
            .and_then(|code| i32::try_from(code).ok())
            .map(Some)
            .ok_or_else(malformed),
    }
}

pub(super) const fn encode_reason(reason: JobStopReason) -> &'static str {
    match reason {
        JobStopReason::Model => "model",
        JobStopReason::User => "user",
        JobStopReason::SessionExit => "session_exit",
        JobStopReason::AttemptEnded => "attempt_ended",
        JobStopReason::HardCap => "hard_cap",
    }
}

pub(super) fn decode_reason(text: &str) -> Result<JobStopReason, ExecutionBackendError> {
    Ok(match text {
        "model" => JobStopReason::Model,
        "user" => JobStopReason::User,
        "session_exit" => JobStopReason::SessionExit,
        "attempt_ended" => JobStopReason::AttemptEnded,
        "hard_cap" => JobStopReason::HardCap,
        _ => return Err(malformed()),
    })
}

/// A stop of one session, or of every job of the line when it names none.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StopWire {
    pub(super) session: Option<String>,
    pub(super) reason: String,
    pub(super) grace_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JobWire {
    number: u32,
    command: String,
    started_at: Timestamp,
    owner: String,
    elapsed_ms: u64,
    status: Value,
    output: String,
    session_id: String,
    owner_generation: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotWire {
    instance: String,
    jobs: Vec<JobWire>,
}

fn encode_job_status(status: JobStatus) -> Value {
    match status {
        JobStatus::Running => json!({ "status": "running" }),
        JobStatus::Exited(code) => json!({ "status": "exited", "code": code }),
        JobStatus::Stopped(reason) => {
            json!({ "status": "stopped", "reason": encode_reason(reason) })
        }
        JobStatus::Released => json!({ "status": "released" }),
        JobStatus::OutcomeUnknown => json!({ "status": "outcome_unknown" }),
    }
}

fn decode_job_status(value: &Value) -> Result<JobStatus, ExecutionBackendError> {
    Ok(match status_name(value)? {
        "running" => JobStatus::Running,
        "exited" => JobStatus::Exited(code(value)?),
        "stopped" => JobStatus::Stopped(decode_reason(
            value
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?,
        )?),
        "released" => JobStatus::Released,
        "outcome_unknown" => JobStatus::OutcomeUnknown,
        _ => return Err(malformed()),
    })
}

pub(super) fn encode_snapshot(
    instance: &str,
    jobs: &[JobView],
) -> Result<Value, ExecutionBackendError> {
    to_value(&SnapshotWire {
        instance: instance.to_owned(),
        jobs: jobs
            .iter()
            .map(|job| JobWire {
                number: job.number,
                command: job.command.clone(),
                started_at: job.started_at,
                owner: job.owner.clone(),
                elapsed_ms: u64::try_from(job.elapsed.as_millis()).unwrap_or(u64::MAX),
                status: encode_job_status(job.status),
                output: job.output.clone(),
                session_id: hex(&job.session_id),
                owner_generation: job.owner_generation,
            })
            .collect(),
    })
}

pub(super) fn decode_snapshot(
    value: Value,
) -> Result<(String, Vec<JobView>), ExecutionBackendError> {
    let wire: SnapshotWire = from_value(value)?;
    let jobs = wire
        .jobs
        .into_iter()
        .map(|job| {
            Ok(JobView {
                number: job.number,
                command: job.command,
                started_at: job.started_at,
                owner: job.owner,
                elapsed: Duration::from_millis(job.elapsed_ms),
                status: decode_job_status(&job.status)?,
                output: job.output,
                session_id: unhex(&job.session_id)?
                    .try_into()
                    .map_err(|_| malformed())?,
                owner_generation: job.owner_generation,
            })
        })
        .collect::<Result<Vec<_>, ExecutionBackendError>>()?;
    Ok((wire.instance, jobs))
}
