//! `proc.*`: process sessions through the canonical process capabilities.
//!
//! `proc.run` and `proc.shell` start a session and wait for it up to their own
//! `timeout_s`, then terminate it. Their children are consumed by the host,
//! so each is acknowledged as soon as it arrives.

use std::time::{Duration, Instant};

use monty_types::{MontyException, MontyObject};
use plakous_tools::{ExecutionValue, ProcessValue};
use serde_json::{Value, json};

use super::{
    args::{HostCall, RunSpec, StartCommand},
    evidence::Job,
    host::{id_error, nested_error, runtime_error, typed, unavailable},
    scope::CellScope,
    values,
};

const OUTPUT_BYTES: u64 = 40_000;
const POLL: &str = "process_session_poll";
const TERMINATE: &str = "process_session_terminate";
const MAX_START_YIELD_MS: u64 = 600_000;
const MAX_WRITE_YIELD_MS: u64 = 300_000;
const MAX_POLL_YIELD_MS: u64 = 3_600_000;

/// Runs one `proc` call.
pub(super) async fn execute(
    call: HostCall,
    scope: &CellScope,
) -> Result<MontyObject, MontyException> {
    match call {
        HostCall::Run(spec) => run(scope, spec).await,
        HostCall::Start { command, cwd, tty } => start(scope, command, cwd, tty).await,
        HostCall::Poll { job } => {
            observe(scope, &job, POLL, |session| poll_input(session, 0, false)).await
        }
        HostCall::Join { job, timeout_ms } => {
            observe(scope, &job, POLL, |session| {
                poll_input(session, timeout_ms.min(MAX_POLL_YIELD_MS), true)
            })
            .await
        }
        HostCall::Cancel { job } => {
            observe(
                scope,
                &job,
                TERMINATE,
                |session| json!({"session_id": session}),
            )
            .await
        }
        HostCall::Stdin { job, data, close } => {
            let session = session_of(scope, &job)?;
            let disposition = if close { "close" } else { "keep_open" };
            child(
                scope,
                "process_session_write",
                json!({"session_id": session, "input": data, "disposition": disposition, "yield_time_ms": 0}),
            )
            .await
            .map(|_| MontyObject::none())
        }
        _ => Err(runtime_error("not a proc function")),
    }
}

/// One child whose process observation the host consumes.
async fn child(
    scope: &CellScope,
    tool: &str,
    input: Value,
) -> Result<ProcessValue, MontyException> {
    let result = scope.call(tool, input).await.map_err(nested_error)?;
    let value = match typed(&result, tool)? {
        ExecutionValue::Process(value) => value.clone(),
        _ => {
            return Err(runtime_error(format!(
                "{tool} returned an unexpected result"
            )));
        }
    };
    result.ack.accept();
    Ok(value)
}

fn start_input(
    command: &StartCommand,
    cwd: Option<&str>,
    tty: bool,
    stdin: bool,
    yield_ms: u64,
) -> (&'static str, Value) {
    let mut input = json!({
        "workdir": cwd.unwrap_or("."),
        "tty": tty,
        "keep_stdin_open": stdin,
        "yield_time_ms": yield_ms,
        "max_output_bytes": OUTPUT_BYTES,
    });
    match command {
        StartCommand::Argv(argv) => {
            input["argv"] = json!(argv);
            ("process_session_exec", input)
        }
        StartCommand::Shell(command) => {
            input["command"] = json!(command);
            input["login"] = json!(false);
            ("process_session_start", input)
        }
    }
}

fn poll_input(session: u64, yield_ms: u64, wait_for_exit: bool) -> Value {
    let mut input = json!({
        "session_id": session,
        "yield_time_ms": yield_ms,
        "max_output_bytes": OUTPUT_BYTES,
        "max_output_records": 256,
    });
    if wait_for_exit {
        input["wait_for_exit"] = json!(true);
    }
    input
}

/// Output gathered across the observations of one run.
#[derive(Default)]
struct Gathered {
    output: String,
    truncated: bool,
    full_output: Option<String>,
}

impl Gathered {
    fn add(&mut self, value: &ProcessValue) {
        self.output.push_str(&value.output);
        self.truncated |= value.truncated;
        if value.full_output.is_some() {
            self.full_output.clone_from(&value.full_output);
        }
    }

    fn omitted(&self, scope: &CellScope) -> Result<Vec<MontyObject>, MontyException> {
        omission(scope, self.truncated, self.full_output.as_deref())
    }
}

fn omission(
    scope: &CellScope,
    truncated: bool,
    full: Option<&str>,
) -> Result<Vec<MontyObject>, MontyException> {
    if !truncated {
        return Ok(Vec::new());
    }
    let full = full
        .map(|path| scope.evidence.process_output(path))
        .transpose()
        .map_err(id_error)?;
    Ok(vec![values::omission("output", 0, 0, full.as_deref())])
}

fn running_session(value: &ProcessValue) -> Option<u64> {
    (value.status == "running")
        .then_some(value.session_id)
        .flatten()
}

fn remaining_ms(deadline: Instant) -> u64 {
    u64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

async fn run(scope: &CellScope, spec: RunSpec) -> Result<MontyObject, MontyException> {
    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
    let first_yield = if spec.stdin.is_some() {
        0
    } else {
        spec.timeout_ms.min(MAX_START_YIELD_MS)
    };
    let (tool, input) = start_input(
        &spec.command,
        spec.cwd.as_deref(),
        false,
        spec.stdin.is_some(),
        first_yield,
    );
    let mut last = child(scope, tool, input).await?;
    let mut gathered = Gathered::default();
    gathered.add(&last);
    if let (Some(data), Some(session)) = (spec.stdin, running_session(&last)) {
        last = child(
            scope,
            "process_session_write",
            json!({
                "session_id": session,
                "input": data,
                "disposition": "close",
                "yield_time_ms": remaining_ms(deadline).min(MAX_WRITE_YIELD_MS),
                "max_output_bytes": OUTPUT_BYTES,
            }),
        )
        .await?;
        gathered.add(&last);
    }
    while let Some(session) = running_session(&last) {
        let remaining = remaining_ms(deadline);
        if remaining == 0 {
            break;
        }
        last = child(
            scope,
            POLL,
            poll_input(session, remaining.min(MAX_POLL_YIELD_MS), true),
        )
        .await?;
        gathered.add(&last);
    }
    let mut timed_out = last.status == "timed_out";
    if let Some(session) = running_session(&last) {
        last = child(scope, TERMINATE, json!({"session_id": session})).await?;
        gathered.add(&last);
        timed_out = true;
    }
    let status = match last.status.as_str() {
        _ if timed_out => "timed_out",
        "exited" => "exited",
        _ => "failed",
    };
    Ok(values::process_result(
        status,
        last.exit_code,
        &gathered.output,
        gathered.omitted(scope)?,
    ))
}

async fn start(
    scope: &CellScope,
    command: StartCommand,
    cwd: Option<String>,
    tty: bool,
) -> Result<MontyObject, MontyException> {
    let (tool, input) = start_input(&command, cwd.as_deref(), tty, tty, 0);
    let value = child(scope, tool, input).await?;
    let job = match running_session(&value) {
        Some(session) => Job::Session(session),
        None => Job::Ended(value),
    };
    let id = scope.evidence.issue_job(job).map_err(id_error)?;
    Ok(values::job(&id))
}

fn session_of(scope: &CellScope, job: &str) -> Result<u64, MontyException> {
    match scope.evidence.job(job) {
        Some(Job::Session(session)) => Ok(session),
        Some(Job::Ended(_)) => Err(runtime_error(format!("job {job} has already ended"))),
        None => Err(unavailable(job)),
    }
}

fn status(
    job: &str,
    value: &ProcessValue,
    scope: &CellScope,
) -> Result<MontyObject, MontyException> {
    let status = match value.status.as_str() {
        "running" => "running",
        "exited" => "exited",
        "outcome_unknown" | "cleanup_unresolved" => "outcome_unknown",
        _ => "stopped",
    };
    Ok(values::process_status(
        job,
        status,
        value.exit_code,
        &value.output,
        omission(scope, value.truncated, value.full_output.as_deref())?,
    ))
}

/// Observes a job with one process child, or reports an ended job's last
/// observation without one.
async fn observe(
    scope: &CellScope,
    job: &str,
    tool: &str,
    input: impl FnOnce(u64) -> Value,
) -> Result<MontyObject, MontyException> {
    let session = match scope.evidence.job(job) {
        None => return Err(unavailable(job)),
        Some(Job::Ended(value)) => return status(job, &value, scope),
        Some(Job::Session(session)) => session,
    };
    let value = child(scope, tool, input(session)).await?;
    status(job, &value, scope)
}
