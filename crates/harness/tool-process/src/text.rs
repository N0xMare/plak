//! The model-visible strings this pack owns.
//!
//! A tool's description and its refusals travel with the tool (spec 02 §6.3):
//! they change together and they are pinned together.

/// Promoted processes are owned by the session, not the invoking turn.
pub const PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT: &str = "A still-running command continues in the background with its session id after the foreground budget; completion is reported automatically. Background jobs survive the turn and stop only on explicit stop, session release, attempt end or the bound hard runtime cap.";

/// Standard write_stdin process-projection tool description.
pub const PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION: &str = "Writes to exec_command's session_id and returns the output that follows. Empty chars waits 5000..300000 ms (default 5000); nonempty chars waits 250..30000 ms (default 250). max_output_tokens: 1..=10000. Background jobs survive the turn; completion is reported automatically.";

/// Codex OAuth `exec_command` tool description (Codex CLI 0.153.x wire).
///
/// Reproduced from OpenAI Codex (Apache-2.0); see the README License section.
pub const PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION: &str = "Run /bin/sh (tty:true: PTY; else pipes). Observation-only yield_time_ms. Head/tail output. `&` children survive shell exit as a job; redirect a service's output to a file. A still-running command continues in the background with its session id after the foreground budget; completion is reported automatically. Background jobs survive the turn and stop only on explicit stop, session release, attempt end or the bound hard runtime cap.";

/// Grok OAuth run_terminal_command tool description.
pub const PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION: &str = "Run a one-shot `/bin/sh -c` command in the session cwd (`.`); cwd and environment do not persist. block_until_ms is an observation hint, not a kill timer, clamped to 600000 ms. Without a hint, use the construction-bound foreground budget (default 30000 ms); 0 selects a 250 ms first observation. When a command moves to the background with an output file, its result and its completion notice carry a `full output: <path>` line; read that file with read_file. Output over 40000 bytes keeps its beginning and its end with the omitted byte count. Await or snapshot process output with get_command_or_subagent_output; stop with kill_command_or_subagent. `&` children survive shell exit as a job; redirect a service's output to a file. A still-running command continues in the background with its session id after the foreground budget; completion is reported automatically. Background jobs survive the turn and stop only on explicit stop, session release, attempt end or the bound hard runtime cap.";

pub(crate) const GROK_GET_COMMAND_OUTPUT_DESCRIPTION: &str = "Get the output so far and status of background processes. Pass task_ids returned by run_terminal_command (N or pN). Completion is reported automatically; pass a positive timeout_ms only when this turn needs the result, to await all requested processes finishing, clamped to 3600000 ms (1 hour). Omit timeout_ms or pass 0 for an immediate snapshot. A steer returns early without interrupting the processes. Results include each task id, output, status and exit code when done. Only terminal observations consume a completion notice; running snapshots do not. Processes only: agents and monitors are not accepted.";

/// Pi `bash`: pi 1.1.0's description, with our shell, bounds and output file.
///
/// Adapted from pi (MIT); see the README License section.
pub(crate) const PI_BASH_DESCRIPTION: &str = "Execute a command through a one-shot `/bin/sh -c` in the session cwd (`.`); cwd and environment do not persist. Returns stdout and stderr. `timeout` is how many seconds to wait for the result, not a kill timer: omit it for the construction-bound foreground budget (default 30000 ms), it is clamped to 600 seconds, and 0 starts the command in the background. When a command moves to the background with an output file, its result and its completion notice carry a `full output: <path>` line; read that file with read. Output over 40000 bytes keeps its beginning and end with the omitted byte count. `&` children survive shell exit as a job; redirect a service's output to a file. A still-running command continues in the background with its session id after the foreground budget; completion is reported automatically. Background jobs survive the turn and stop only on explicit stop, session release, attempt end or the bound hard runtime cap.";

/// Pi `wait` where the node has no agent tools: own processes only. On a
/// node granted agent tools the tree's `wait` claims the name.
pub(crate) const PI_WAIT_DESCRIPTION: &str = "Wait for background processes started by bash. Pass ids (N or pN), or omit them for every unreported background process (one that already finished returns at once). Returns when the first of them finishes, a message steers you, or timeout_ms passes, with each process's status, exit code and output so far; an id already reported does not end the wait. Completion is reported automatically: use wait only when this turn needs a result. Only finished processes consume their completion notice. Processes only: agents are not accepted.";

/// Pi `stop` where the node has no agent tools: own processes only. On a
/// node granted agent tools the tree's `stop` claims the name.
pub(crate) const PI_STOP_DESCRIPTION: &str = "Stop a background process by the id bash returned (N or pN). Sends SIGTERM to its process group, then SIGKILL after a 2 second grace. Succeeds if the process stopped or had already exited; a natural exit keeps its original status. Processes only: agents are not accepted.";

pub(crate) const GROK_KILL_COMMAND_DESCRIPTION: &str = "Stop a background process by task_id returned by run_terminal_command (N or pN). Send SIGTERM to its process group, then SIGKILL after a 2 second grace. Succeeds if the process stopped or had already exited; a natural exit keeps its original status. Processes only: agents and monitors are not accepted.";

/// Joined output over the page cap: the head, this marker, then the tail.
/// `{omitted}` is replaced by the omitted byte count.
pub const PLAKOUS_TOOL_OUTPUT_OMITTED: &str = "\n[... {omitted} bytes of output omitted ...]\n";

/// Last line of a promotion result or a summarized page of a job with an
/// output file; the path follows.
pub(crate) const PROCESS_OUTPUT_FILE: &str = "full output: ";

/// Renders [`PLAKOUS_TOOL_OUTPUT_OMITTED`] for `omitted` bytes.
#[must_use]
pub fn output_omitted_marker(omitted: u64) -> String {
    PLAKOUS_TOOL_OUTPUT_OMITTED.replace("{omitted}", &omitted.to_string())
}

const LIFETIME_TAIL_LEAD: &str = "Construction-bound foreground budget:";

/// The bound job-lifetime tail [`bound_description`] appended to
/// `description`, through the end, when it has one.
pub(crate) fn lifetime_tail(description: &str) -> Option<&str> {
    description
        .find(LIFETIME_TAIL_LEAD)
        .map(|index| description[index..].trim_end())
}

pub(crate) fn bound_description(
    description: &str,
    config: plakous_tools::JobConfig,
    kind: crate::ProcessToolProjectionKind,
    actions: bool,
) -> String {
    let actions = if actions {
        match kind {
            crate::ProcessToolProjectionKind::CodexUnifiedExec => {
                " Read: exec_command with session_id, no cmd; wait/read: write_stdin with empty chars; stop: write_stdin with chars exactly \\u0003."
            }
            crate::ProcessToolProjectionKind::ClaudeOAuthBash => {
                " Wait by giving a long command a timeout long enough to finish; stop with TaskStop using task_id."
            }
            crate::ProcessToolProjectionKind::PiBash => {
                " Wait with wait; stop with stop using the id."
            }
            crate::ProcessToolProjectionKind::GrokTerminal => {
                " Read with get_command_or_subagent_output using timeout_ms: 0; wait with a positive timeout_ms; stop with kill_command_or_subagent using task_id."
            }
        }
    } else {
        ""
    };
    let description = description
        .replace(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT, "")
        .replace(
            "Background jobs survive the turn; completion is reported automatically.",
            "",
        )
        .replace(
            "(default 30000 ms)",
            &match kind {
                crate::ProcessToolProjectionKind::PiBash => {
                    format!("({} seconds)", config.foreground_seconds())
                }
                _ => format!("({} ms)", config.foreground_seconds() * 1000),
            },
        );
    let description = description.trim_end();
    format!(
        "{description} {LIFETIME_TAIL_LEAD} {} seconds ({} ms); hard runtime cap: {} hours. {PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT}{actions}",
        config.foreground_seconds(),
        config.foreground_seconds() * 1000,
        config.max_runtime_hours(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetime_tail_is_the_rendered_bound_suffix() {
        for kind in [
            crate::ProcessToolProjectionKind::PiBash,
            crate::ProcessToolProjectionKind::CodexUnifiedExec,
            crate::ProcessToolProjectionKind::GrokTerminal,
            crate::ProcessToolProjectionKind::ClaudeOAuthBash,
        ] {
            for actions in [false, true] {
                let rendered = bound_description(
                    "Reads job output.",
                    plakous_tools::JobConfig::default(),
                    kind,
                    actions,
                );
                let tail = lifetime_tail(&rendered).unwrap_or_default();
                assert!(tail.starts_with(LIFETIME_TAIL_LEAD), "{rendered}");
                assert!(tail.contains("hard runtime cap"), "{tail}");
                assert!(rendered.trim_end().ends_with(tail), "{rendered}");
            }
        }
        assert_eq!(lifetime_tail("Reads job output."), None);
    }
}
