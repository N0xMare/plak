use super::ToolOperation;
use serde_json::Value;

impl ToolOperation {
    /// Shared live/reconstructed process meaning, including exposed-route
    /// defaults. Only structured argument objects carry argument facts.
    #[must_use]
    pub fn process_operation(name: &str, exposed: Option<&str>, args: &Value) -> Option<Self> {
        args.as_object()?;
        if matches!(exposed.unwrap_or(name), "wait" | "wait_jobs")
            && matches!(name, "process_session_poll" | "wait" | "wait_jobs")
            && args.get("cell_id").is_none()
        {
            return Some(read_or_wait(
                args.get("timeout_ms").and_then(Value::as_u64) == Some(0),
            ));
        }
        exposed
            .and_then(|name| process_route(name, args))
            .or_else(|| process_route(name, args))
    }

    /// The product's live subject label for a process operation.
    #[must_use]
    pub const fn process_label(self) -> Option<&'static str> {
        match self {
            Self::Run => Some("Shell"),
            Self::ReadProcess => Some("process_session_read"),
            Self::WaitProcess => Some("process_session_wait"),
            Self::InputProcess => Some("process_session_write"),
            Self::InterruptProcess => Some("process_session_interrupt"),
            Self::StopProcess => Some("process_session_terminate"),
            _ => None,
        }
    }
}

fn read_or_wait(read: bool) -> ToolOperation {
    if read {
        ToolOperation::ReadProcess
    } else {
        ToolOperation::WaitProcess
    }
}

fn process_route(name: &str, args: &Value) -> Option<ToolOperation> {
    use ToolOperation as O;
    let zero_yield = args.get("yield_time_ms").and_then(Value::as_u64) == Some(0);
    let nonblocking = args.get("block").and_then(Value::as_bool) == Some(false);
    Some(match name {
        "process_session_start"
        | "run_process"
        | "run_terminal_cmd"
        | "run_terminal_command"
        | "Bash"
        | "bash"
        | "Shell" => O::Run,
        "exec_command"
            if args
                .get("cmd")
                .or_else(|| args.get("command"))
                .and_then(Value::as_str)
                .is_some_and(|cmd| !cmd.is_empty()) =>
        {
            O::Run
        }
        "exec_command" => read_or_wait(zero_yield),
        "write_stdin" | "process_session_write" => match args
            .get("input")
            .or_else(|| args.get("chars"))
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "\u{3}" => O::InterruptProcess,
            "" => O::WaitProcess,
            _ => O::InputProcess,
        },
        "process_session_read" => O::ReadProcess,
        "process_session_wait" => O::WaitProcess,
        "process_session_interrupt" => O::InterruptProcess,
        "TaskStop" | "KillShell" | "kill_command_or_subagent" | "process_session_terminate" => {
            O::StopProcess
        }
        "stop" if args.get("id").is_some() => O::StopProcess,
        "BashOutput" => read_or_wait(nonblocking || zero_yield),
        "process_session_poll" | "get_command_or_subagent_output" => read_or_wait(
            nonblocking
                || args
                    .get("yield_time_ms")
                    .or_else(|| args.get("timeout_ms"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    == 0,
        ),
        _ => return None,
    })
}
