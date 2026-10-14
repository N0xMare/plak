//! Long-lived stdio client of [`plakous_hosted::runtime::spawn_host_worker`].

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::surface_protocol::{HostEvent, UserAction, UserActionSendError};

use crate::agent_jsonl::{
    self, AgentCommand, JsonlCorrelation, ParseOutcome, bye_line, diagnostic_line, hello_line,
    parse_command_line, project_host_event, ready_line,
};
use crate::topology_product::durable_topology_tree_id;
use plakous_hosted::agent::RESTORED_PAUSED_RESUME_HINT;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use plakous_hosted::runtime::{HostConfig, effort_label, spawn_host_worker};
use plakous_hosted::topology_runtime;

const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_POLL: Duration = Duration::from_millis(20);
const ACTION_RETRY: Duration = Duration::from_secs(2);
const RESUME_WAIT: Duration = Duration::from_secs(5);
const IDLE_STEER_MESSAGE: &str = "steer is only valid while a turn is busy";

/// Attach the product host and drive it from JSONL stdin until quit or EOF.
pub fn run_agent_stdio(
    config: HostConfig<crate::surface_protocol::PlakExtension>,
    resume_tree: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let outcome = run_agent_stdio_with_input(
        config,
        resume_tree,
        stdout,
        stderr,
        std::io::BufReader::new(std::io::stdin()),
    );
    // A failure between admission and the drive's own tail — a session that
    // will not reopen is the one that happens — would otherwise leave the
    // reader at end of stream with an empty stdout, which is the shape a killed
    // process has. The drive terminates its own stream on every path it
    // reaches, so this only fires when it reached none of them, and the claim
    // is what guarantees the reader still sees exactly one terminator.
    if let Err(failure) = &outcome
        && crate::signals::claim_terminal_record()
    {
        let _ = writeln!(stdout, "{}", diagnostic_line(None, failure.message()));
        let _ = writeln!(stdout, "{}", bye_line(None));
        let _ = stdout.flush();
    }
    outcome
}

fn run_agent_stdio_with_input(
    config: HostConfig<crate::surface_protocol::PlakExtension>,
    resume_tree: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    input: impl BufRead + Send + 'static,
) -> CliResult<ProcessOutcome> {
    run_agent_stdio_stopping_on(
        config,
        resume_tree,
        stdout,
        stderr,
        input,
        crate::signals::stop_reason,
        crate::signals::interrupts,
    )
}

/// The drive, with the signal record supplied rather than read from the
/// process.
///
/// The record a terminating signal leaves is process-wide, which is right for a
/// process that is ending and wrong for a test that shares one. Reading it at
/// the single entry point above and handing the answer in keeps the drive a
/// function of its arguments.
fn run_agent_stdio_stopping_on(
    mut config: HostConfig<crate::surface_protocol::PlakExtension>,
    resume_tree: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    input: impl BufRead + Send + 'static,
    stop_reason: fn() -> Option<&'static str>,
    interrupts: fn() -> usize,
) -> CliResult<ProcessOutcome> {
    let mut stdout = Output {
        writer: stdout,
        failed: false,
    };
    config.auto_continue = false;
    config.interactive = false;
    let topology_enabled = config.live_topology_enabled()?;
    let session_id = config.session.id.clone();
    let state_roots = config.state_roots.clone();
    let telemetry = state_roots.product_telemetry();
    let tree_id = durable_topology_tree_id(&state_roots, &config.session);
    let account = config.session.account.clone();
    let model = Some(config.model_id.clone());
    let effort = config.effort.map(effort_label);
    let composition = config.resolved_composition_label().to_owned();

    let process_started = Instant::now();
    let (actions, events, worker) = spawn_host_worker(config)?;
    // A supervisor's stop, or a closed terminal, must not end the stream
    // without a terminal record, and must not end it on a stale branch head.
    // The stdio drive never polls anything but its event and stdin channels, so
    // a signal is delivered the only way this face can act on one: as the same
    // interrupt-then-quit intent an `EOF` or a `quit` command sends. The drive
    // then tears down, checkpoints and writes its own `error` plus `bye` on the
    // ordinary path. The watcher's own record and its hard exit remain, but
    // only as the fallback for a teardown that outruns the deadline.
    let (stdin_tx, stdin_rx) = mpsc::sync_channel::<std::io::Result<Option<String>>>(32);
    let stdin_thread = thread::spawn(move || {
        let mut input = input;
        loop {
            let frame = read_frame(&mut input, agent_jsonl::MAX_FRAME_BYTES);
            let terminal = !matches!(&frame, Ok(Some(_)));
            if stdin_tx.send(frame).is_err() || terminal {
                break;
            }
        }
    });

    let mut busy = false;
    #[cfg(feature = "test-hooks")]
    let idle_gate = crate::test_hooks::assistant_done_release();
    #[cfg(feature = "test-hooks")]
    let mut wait_for_idle = false;
    #[cfg(feature = "test-hooks")]
    let mut wait_for_input = false;
    let mut attached = false;
    let mut hello_sent = false;
    let mut shutting_down = false;
    // Set when a `SIGINT` cut a running turn short.
    let mut interrupted_turn = false;
    let mut prompt_id: Option<String> = None;
    let mut prompt_origins = PromptOrigins::default();
    let mut command_id: Option<String> = None;
    // The `quit` command's own id, for the stream's terminal records.
    let mut quit_id: Option<String> = None;
    let mut pending_tree_snapshot_id: Option<String> = None;
    let mut tree_state = "unknown".to_owned();
    let mut turn_busy = false;
    let mut shutdown_started = Instant::now();
    let mut attached_at = Instant::now();
    let mut pending_stdout: Vec<String> = Vec::new();

    let mut resume_sent = false;
    let mut navigator_opened = false;
    let mut pending_command: Option<(
        Instant,
        plakous_hosted::product_observation::JsonlCommandKind,
    )> = None;
    let mut refuse: Option<String> = None;

    // A panic anywhere in the drive would unwind past the terminal record and
    // leave a JSONL client at end of stream with no `bye` and no `error` —
    // indistinguishable from the process being killed. Catching it here does
    // not change the failure: the unwind resumes immediately after the stream
    // has been terminated properly.
    let mut panicked = None;
    let drive_result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> CliResult<()> {
            loop {
                match events.recv_timeout(EVENT_POLL) {
                    Ok(event) => {
                        #[cfg(feature = "test-hooks")]
                        if matches!(event, HostEvent::SetBusy(false)) {
                            wait_for_idle = false;
                        }
                        prompt_origins.observe(&event, &mut prompt_id);
                        // Answers belong to the command the host is handling,
                        // announced at the boundary, never to whichever command
                        // stdin happened to read last.
                        match &event {
                            HostEvent::IntentStarted(started) => command_id.clone_from(started),
                            HostEvent::PromptCorrelationReset => command_id = None,
                            _ => {}
                        }
                        let lines = if let Some(id) = prompt_origins.take_tool_origin(&event) {
                            project_host_event(
                                &event,
                                JsonlCorrelation {
                                    prompt_id: Some(&id),
                                    ..JsonlCorrelation::default()
                                },
                            )
                            .into_iter()
                            .collect()
                        } else {
                            handle_event(
                                &event,
                                &mut busy,
                                &mut turn_busy,
                                &mut tree_state,
                                &mut prompt_id,
                                command_id.as_ref(),
                                &mut pending_tree_snapshot_id,
                            )
                        };
                        if !lines.is_empty() {
                            let byte_len =
                                u64::try_from(lines.iter().map(String::len).sum::<usize>())
                                    .unwrap_or(u64::MAX);
                            let _event_span = tracing::info_span!(
                                "jsonl.event_batch",
                                kind = agent_jsonl::host_event_kind(&event),
                                byte_len,
                            )
                            .entered();
                        }
                        if !lines.is_empty()
                            && let Some((started, kind)) = pending_command.take()
                            && let Some(session_id) =
                                plakous_hosted::product_observation::session_id_from_text(
                                    &session_id,
                                )
                        {
                            telemetry.emit(
                            plakous_hosted::product_observation::ProductObservation::JsonlCommand {
                                at: plakous_hosted::product_observation::wall_timestamp(),
                                session_id,
                                kind,
                                admitted_to_completed: started.elapsed(),
                            },
                        );
                        }
                        emit_lines(hello_sent, &mut pending_stdout, &mut stdout, lines)?;
                        #[cfg(feature = "test-hooks")]
                        if matches!(event, HostEvent::AssistantDone)
                            && let Some(gate) = &idle_gate
                        {
                            while !gate.exists() {
                                thread::sleep(Duration::from_millis(1));
                            }
                            let mode = std::fs::read(gate).unwrap_or_default();
                            wait_for_input = mode == b"quit";
                            wait_for_idle = mode == b"idle";
                        }
                        if matches!(event, HostEvent::AgentAttached(true)) {
                            attached = true;
                            attached_at = Instant::now();
                            let need_resume = resume_tree && !resume_sent;
                            let need_navigator = !navigator_opened;
                            // A home may enable orchestration before this
                            // session has ever created a tree. The idle host
                            // pushes its first tree revision if one appears.
                            let hub_attached = topology_runtime::lookup(&tree_id).is_some();
                            for action in actions_on_successful_attach(
                                need_resume,
                                need_navigator,
                                topology_enabled && hub_attached,
                            ) {
                                send_action(&actions, action)?;
                            }
                            if need_resume {
                                resume_sent = true;
                            }
                            if need_navigator && topology_enabled && hub_attached {
                                navigator_opened = true;
                            }
                        }
                        if matches!(event, HostEvent::AgentAttached(false)) {
                            attached = true;
                            attached_at = Instant::now();
                            shutting_down = true;
                            shutdown_started = Instant::now();
                            begin_shutdown(&actions, busy)?;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if attached && !hello_sent && refuse.is_none() {
                    // Admission resolved `--resume-tree` against the durable tree.
                    // If that tree exists, a missing hub must wait for its
                    // registration and explicit resume before hello.
                    let paused = topology_runtime::hub_tree_is_restored_paused(&tree_id)
                        .unwrap_or(resume_tree);
                    match hello_gate(resume_tree, paused, attached_at.elapsed() >= RESUME_WAIT) {
                        HelloGate::Wait => {}
                        HelloGate::RefusePaused => {
                            refuse = Some(RESTORED_PAUSED_RESUME_HINT.to_owned());
                            write_line(
                                &mut stdout,
                                &diagnostic_line(None, RESTORED_PAUSED_RESUME_HINT),
                            )?;
                            shutting_down = true;
                            shutdown_started = Instant::now();
                            begin_shutdown(&actions, busy)?;
                        }
                        HelloGate::Hello => {
                            if tree_state == "unknown" {
                                tree_state = "live".to_owned();
                            }
                            let (javascript_host, javascript_host_target) =
                                crate::sidecars::js_host::hello_status();
                            let (python_host, python_host_target) =
                                crate::sidecars::python_bundle::python_host_hello_status();
                            let elapsed_ms = u64::try_from(process_started.elapsed().as_millis())
                                .unwrap_or(u64::MAX);
                            let handshake = format!(
                                "{}\n{}",
                                hello_line(
                                    None,
                                    &session_id,
                                    &tree_id,
                                    account.as_deref(),
                                    model.as_deref(),
                                    effort.as_deref(),
                                    &composition,
                                    &tree_state,
                                    Some(&javascript_host),
                                    Some(javascript_host_target),
                                    Some(&python_host),
                                    Some(python_host_target),
                                    Some(elapsed_ms),
                                ),
                                ready_line(None, Some(elapsed_ms))
                            );
                            writeln!(stdout, "{handshake}")
                                .map_err(|_| CliFailure::new("stdout failed"))?;
                            stdout
                                .flush()
                                .map_err(|_| CliFailure::new("stdout failed"))?;
                            for line in pending_stdout.drain(..) {
                                write_line(&mut stdout, &line)?;
                            }
                            hello_sent = true;
                        }
                    }
                }

                // A signal stops this face the way end of input does. Ctrl-C
                // interrupts a running turn first; idle, it is a plain quit.
                if !shutting_down && (stop_reason().is_some() || interrupts() > 0) {
                    shutting_down = true;
                    shutdown_started = Instant::now();
                    interrupted_turn = busy && interrupts() > 0;
                    begin_shutdown(&actions, busy)?;
                }
                if shutting_down {
                    if worker.is_finished() {
                        break;
                    }
                    if shutdown_started.elapsed() > JOIN_TIMEOUT {
                        break;
                    }
                    continue;
                }
                if !hello_sent {
                    // Leave startup input in the bounded channel until the handshake.
                    continue;
                }
                #[cfg(feature = "test-hooks")]
                if wait_for_idle {
                    continue;
                }

                #[cfg(feature = "test-hooks")]
                let received = if wait_for_input {
                    wait_for_input = false;
                    stdin_rx.recv().map_err(|_| TryRecvError::Disconnected)
                } else {
                    stdin_rx.try_recv()
                };
                #[cfg(not(feature = "test-hooks"))]
                let received = stdin_rx.try_recv();
                let next_stdin = match received {
                    Ok(frame) => Some(
                        frame.map_err(|error| CliFailure::new(format!("stdin failed: {error}")))?,
                    ),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(None),
                };
                match next_stdin {
                    Some(None) => {
                        shutting_down = true;
                        shutdown_started = Instant::now();
                        begin_shutdown(&actions, busy)?;
                    }
                    Some(Some(line)) => match parse_command_line(&line) {
                        ParseOutcome::Empty => {}
                        ParseOutcome::Diagnostic { id, message } => {
                            let _ = writeln!(stderr, "plak agent: {message}");
                            write_line(&mut stdout, &diagnostic_line(id.as_deref(), &message))?;
                            if let Some(session_id) =
                                plakous_hosted::product_observation::session_id_from_text(
                                    &session_id,
                                )
                                && let Some((started, kind)) = pending_command.take()
                            {
                                telemetry.emit(
                            plakous_hosted::product_observation::ProductObservation::JsonlCommand {
                                at: plakous_hosted::product_observation::wall_timestamp(),
                                session_id,
                                kind,
                                admitted_to_completed: started.elapsed(),
                            },
                        );
                            }
                        }
                        ParseOutcome::Command(command) => {
                            let cmd_id = command.id().to_owned();
                            pending_command = Some((
                        Instant::now(),
                        plakous_hosted::product_observation::JsonlCommandKind::from_command_name(
                            command.kind_name(),
                        ),
                    ));
                            let _command_span = tracing::info_span!(
                                "jsonl.command",
                                kind = command.kind_name(),
                                session_id = %session_id,
                            )
                            .entered();
                            match &command {
                                AgentCommand::Tree { .. } | AgentCommand::ResumeTree { .. } => {
                                    pending_tree_snapshot_id = Some(cmd_id.clone());
                                }
                                _ => {}
                            }
                            if matches!(command, AgentCommand::Quit { .. }) {
                                quit_id = Some(cmd_id.clone());
                                shutting_down = true;
                                shutdown_started = Instant::now();
                                begin_shutdown(&actions, busy)?;
                            } else if let AgentCommand::Prompt { steer: true, .. } = &command {
                                if busy {
                                    if let Some(action) = command.into_user_action(busy) {
                                        send_correlated_action(&actions, action, cmd_id)?;
                                    }
                                } else {
                                    let _ = writeln!(stderr, "plak agent: {IDLE_STEER_MESSAGE}");
                                    write_line(
                                        &mut stdout,
                                        &diagnostic_line(Some(&cmd_id), IDLE_STEER_MESSAGE),
                                    )?;
                                    if let Some(session_id) =
                                        plakous_hosted::product_observation::session_id_from_text(
                                            &session_id,
                                        )
                                        && let Some((started, kind)) = pending_command.take()
                                    {
                                        telemetry.emit(
                                    plakous_hosted::product_observation::ProductObservation::JsonlCommand {
                                        at: plakous_hosted::product_observation::wall_timestamp(),
                                        session_id,
                                        kind,
                                        admitted_to_completed: started.elapsed(),
                                    },
                                );
                                    }
                                }
                            } else if let Some(action) = command.into_user_action(busy) {
                                prompt_origins.submitted(&action, cmd_id.clone());
                                send_correlated_action(&actions, action, cmd_id)?;
                            }
                        }
                    },
                    None => {}
                }
            }

            if let Some(message) = refuse {
                return Err(CliFailure::new(message));
            }
            Ok(())
        },
    )) {
        Ok(result) => result,
        Err(payload) => {
            panicked = Some(payload);
            Err(CliFailure::new("plak agent host panicked"))
        }
    };

    // Every fallible drive path returns here while the worker is still owned.
    // Admission, event draining and the join share the original shutdown bound.
    let deadline = if shutting_down {
        shutdown_started + JOIN_TIMEOUT
    } else {
        Instant::now() + JOIN_TIMEOUT
    };
    let shutdown_result = if !shutting_down && !worker.is_finished() {
        begin_shutdown_until(&actions, busy, deadline)
    } else {
        Ok(())
    };
    drop(actions);
    drop(stdin_rx);
    let join_result = join_worker(worker, deadline);
    if stdin_thread.is_finished() {
        let _ = stdin_thread.join();
    }
    // A blocking OS stdin read cannot be interrupted by dropping its receiver.
    // It owns no host state; never extend host cleanup to wait for input.
    // One terminal record on every path. A failed writer is the sole
    // exception, and only because the stream is already gone.
    // One source of truth for "this stream has been terminated". The signal
    // face reads the same flag before writing its own record, so a stop that
    // races a natural exit cannot produce two `bye` lines.
    crate::signals::terminal_record_written();
    // Whichever stage failed, the client is told. A teardown that fails after
    // the stream has started — a child that will not release, a worker past
    // its join deadline — used to exit non-zero with a terminated stream that
    // said nothing, which is only marginally better than no terminator at all.
    let reason = drive_result
        .as_ref()
        .err()
        .or_else(|| shutdown_result.as_ref().err())
        .or_else(|| join_result.as_ref().err());
    let bye_result = if stdout.failed {
        Ok(())
    } else {
        // A signalled stop is the reason even when every stage succeeded: the
        // teardown worked precisely because the stop was asked for, and a
        // reader that saw only `bye` could not tell the turn was cut short.
        if let Some(error) = reason {
            let _ = write_line(
                &mut stdout,
                &diagnostic_line(command_id.as_deref(), error.message()),
            );
        } else if let Some(message) = stop_reason() {
            let _ = write_line(&mut stdout, &diagnostic_line(None, message));
        } else if interrupted_turn {
            let _ = write_line(
                &mut stdout,
                &diagnostic_line(
                    None,
                    "plak was interrupted (SIGINT) before the turn completed",
                ),
            );
        }
        write_line(&mut stdout, &bye_line(quit_id.as_deref()))
    };
    if let Some(payload) = panicked {
        std::panic::resume_unwind(payload);
    }
    combine_results(
        drive_result,
        combine_results(combine_results(shutdown_result, join_result), bye_result),
    )?;
    if stop_reason().is_some() || interrupted_turn {
        return Ok(ProcessOutcome::Interrupted);
    }
    Ok(ProcessOutcome::Success)
}

struct Output<'a> {
    writer: &'a mut dyn Write,
    failed: bool,
}

/// How long to keep retrying a stdout that is momentarily unwritable.
///
/// A JSONL client that reads slowly fills the pipe; a non-blocking stdout then
/// answers `WouldBlock` and a zero-progress write. Treating either as fatal
/// poisoned the stream permanently, and because a poisoned stream skips the
/// terminal record, the client saw the transcript stop mid-way with no `bye`
/// and no `error` — the live early-exit shape, with a normal exit code 1 and
/// nothing killed. Waiting for the reader is the correct response; giving up
/// on the stream is not.
const OUTPUT_STALL_BUDGET: Duration = Duration::from_secs(20);

const OUTPUT_STALL_BACKOFF: Duration = Duration::from_millis(2);

impl Write for Output<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + OUTPUT_STALL_BUDGET;
        loop {
            match self.writer.write(bytes) {
                // Zero progress on a non-empty buffer is the same stall a
                // `WouldBlock` reports, just spelled differently.
                Ok(0) | Err(_) if Instant::now() >= deadline => {
                    self.failed = true;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "stdout stalled beyond its budget",
                    ));
                }
                Ok(0) => thread::sleep(OUTPUT_STALL_BACKOFF),
                Ok(written) => return Ok(written),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        thread::sleep(OUTPUT_STALL_BACKOFF);
                    }
                }
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let deadline = Instant::now() + OUTPUT_STALL_BUDGET;
        loop {
            match self.writer.flush() {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) && Instant::now() < deadline =>
                {
                    thread::sleep(OUTPUT_STALL_BACKOFF);
                }
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
    }
}

fn combine_results(primary: CliResult<()>, cleanup: CliResult<()>) -> CliResult<()> {
    match (primary, cleanup) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(primary), Err(cleanup)) => Err(CliFailure::new(format!(
            "{}; cleanup: {}",
            primary.message(),
            cleanup.message()
        ))),
    }
}

/// Consume at most one bounded frame, including its optional CR and newline.
/// Overflow disconnects immediately: draining an unterminated peer stream is
/// unbounded work. The reader never allocates beyond the frame envelope.
fn read_frame(input: &mut impl BufRead, limit: usize) -> std::io::Result<Option<String>> {
    let mut frame = Vec::with_capacity(limit);
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            break;
        }
        let inspected = available.len().min(limit.saturating_sub(frame.len()) + 1);
        let newline = available[..inspected]
            .iter()
            .position(|byte| *byte == b'\n');
        let count = newline.map_or(inspected, |index| index + 1);
        if count > limit.saturating_sub(frame.len()) {
            return Err(std::io::Error::other(
                "JSONL frame exceeds 1 MiB; disconnecting",
            ));
        }
        frame.extend_from_slice(&available[..count]);
        input.consume(count);
        if newline.is_some() {
            break;
        }
    }
    String::from_utf8(frame)
        .map(Some)
        .map_err(std::io::Error::other)
}

pub(crate) fn actions_on_successful_attach(
    send_resume: bool,
    send_navigator: bool,
    topology_available: bool,
) -> Vec<UserAction> {
    let mut actions = Vec::new();
    if send_resume {
        actions.push(UserAction::ResumeTopologyTree);
    }
    if send_navigator && topology_available {
        actions.push(UserAction::OpenTopologyNavigator);
    }
    actions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelloGate {
    Wait,
    Hello,
    RefusePaused,
}

fn hello_gate(resume_tree: bool, paused: bool, wait_elapsed: bool) -> HelloGate {
    match (paused, resume_tree, wait_elapsed) {
        (true, false, _) | (true, true, true) => HelloGate::RefusePaused,
        (true, true, false) => HelloGate::Wait,
        (false, _, _) => HelloGate::Hello,
    }
}

fn emit_lines(
    hello_sent: bool,
    pending: &mut Vec<String>,
    stdout: &mut dyn Write,
    lines: Vec<String>,
) -> CliResult<()> {
    for line in lines {
        if hello_sent {
            write_line(stdout, &line)?;
        } else {
            pending.push(line);
        }
    }
    Ok(())
}

fn handle_event(
    event: &HostEvent,
    busy: &mut bool,
    turn_busy: &mut bool,
    tree_state: &mut String,
    prompt_id: &mut Option<String>,
    command_id: Option<&String>,
    pending_tree_snapshot_id: &mut Option<String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let HostEvent::SetBusy(value) = event {
        *turn_busy = *value;
        *busy = *value;
    }
    if let HostEvent::TopologyNavigatorSnapshot(snapshot) = event {
        *tree_state = snapshot.state.as_str().to_owned();
    }
    let correlation = JsonlCorrelation {
        prompt_id: prompt_id.as_deref(),
        omit_unknown_turn_id: true,
        command_id: command_id.map(String::as_str),
        pending_tree_snapshot_id: pending_tree_snapshot_id.as_deref(),
    };
    if let Some(line) = project_host_event(event, correlation) {
        lines.push(line);
        if let HostEvent::TopologyNavigatorSnapshot(snapshot) = event
            && snapshot.solicited
        {
            *pending_tree_snapshot_id = None;
        }
    }
    if matches!(event, HostEvent::SetBusy(false)) {
        *prompt_id = None;
    }
    lines
}

#[derive(Default)]
struct PromptOrigins {
    pending: VecDeque<(String, String)>,
    admitted: Vec<(plakous_host_protocol::QueueItem, String)>,
    tool_origin: Option<(String, String)>,
    current_intent: Option<String>,
    disabled: bool,
}

impl PromptOrigins {
    fn submitted(&mut self, action: &UserAction, id: String) {
        if !self.disabled
            && let UserAction::Prompt(text) | UserAction::FollowUp(text) = action
        {
            self.pending.push_back((text.trim().to_owned(), id));
        }
    }

    fn take_tool_origin(&mut self, event: &HostEvent) -> Option<String> {
        let HostEvent::ToolLine {
            call_id: Some(call_id),
            state,
            ..
        } = event
        else {
            return None;
        };
        let (id, command) = self.tool_origin.as_ref()?;
        if id != call_id {
            return None;
        }
        let command = command.clone();
        if state != "start" {
            self.tool_origin = None;
        }
        Some(command)
    }

    fn observe(&mut self, event: &HostEvent, active: &mut Option<String>) {
        if let HostEvent::ToolInvocationOrigin {
            call_id,
            command_id,
        } = event
        {
            // Operator invocations execute serially: one bounded slot suffices
            // even when a terminal tool observation was lost.
            self.tool_origin = Some((call_id.clone(), command_id.clone()));
        }
        if matches!(event, HostEvent::PromptCorrelationReset) {
            // Old receipts may remain buffered while new stdin arrives. Without
            // a complete receipt stream, this attachment cannot infer origins again.
            self.disabled = true;
            self.pending.clear();
            self.admitted.clear();
            self.tool_origin = None;
            *active = None;
        }
        if matches!(
            event,
            HostEvent::ClearView | HostEvent::SessionOpened { .. }
        ) {
            self.tool_origin = None;
        }
        if self.disabled {
            return;
        }
        match event {
            HostEvent::IntentStarted(id) => self.current_intent.clone_from(id),
            HostEvent::PromptAdmitted(item) => {
                if let Some(index) = self.pending.iter().position(|(text, _)| *text == item.text)
                    && let Some((_, id)) = self.pending.remove(index)
                {
                    self.admitted.push((item.clone(), id));
                }
            }
            HostEvent::PromptStarted(item) => {
                // Busy snapshots can coalesce across adjacent turns. This receipt
                // always replaces the previous origin, including unknown/restored work.
                *active = item.as_ref().and_then(|item| {
                    self.admitted
                        .iter()
                        .position(|(admitted, _)| admitted == item)
                        .map(|index| self.admitted.remove(index).1)
                });
            }
            HostEvent::DraftRejected(UserAction::Prompt(text) | UserAction::FollowUp(text)) => {
                if let Some(index) = self.pending.iter().position(|(pending, id)| {
                    pending == text.trim() && self.current_intent.as_ref() == Some(id)
                }) {
                    self.pending.remove(index);
                }
            }
            HostEvent::QueueSnapshot { items, .. } => {
                self.admitted
                    .retain(|(admitted, _)| items.contains(admitted));
            }
            HostEvent::ClearView | HostEvent::SessionOpened { .. } => {
                self.pending.clear();
                self.admitted.clear();
                *active = None;
            }
            _ => {}
        }
    }
}

fn begin_shutdown(
    actions: &crate::surface_protocol::UserActionSender,
    busy: bool,
) -> CliResult<()> {
    begin_shutdown_until(actions, busy, Instant::now() + ACTION_RETRY)
}

fn begin_shutdown_until(
    actions: &crate::surface_protocol::UserActionSender,
    busy: bool,
    deadline: Instant,
) -> CliResult<()> {
    let interrupt = if busy {
        send_action_until(actions, UserAction::Interrupt, deadline)
    } else {
        Ok(())
    };
    let quit = send_action_until(actions, UserAction::Quit, deadline);
    combine_results(interrupt, quit)
}

fn send_action(
    actions: &crate::surface_protocol::UserActionSender,
    action: UserAction,
) -> CliResult<()> {
    send_action_until(actions, action, Instant::now() + ACTION_RETRY)
}

/// Sends a stdin command's action tagged with its id, which the host echoes
/// when it starts handling it.
fn send_correlated_action(
    actions: &crate::surface_protocol::UserActionSender,
    action: UserAction,
    command_id: String,
) -> CliResult<()> {
    send_action_until_with(
        actions,
        action,
        Some(command_id),
        Instant::now() + ACTION_RETRY,
    )
}

fn send_action_until(
    actions: &crate::surface_protocol::UserActionSender,
    action: UserAction,
    deadline: Instant,
) -> CliResult<()> {
    send_action_until_with(actions, action, None, deadline)
}

fn send_action_until_with(
    actions: &crate::surface_protocol::UserActionSender,
    mut action: UserAction,
    correlation: Option<String>,
    deadline: Instant,
) -> CliResult<()> {
    loop {
        match actions.send_correlated(action, correlation.clone()) {
            Ok(()) => return Ok(()),
            Err(UserActionSendError::Duplicate(returned)) if Instant::now() < deadline => {
                thread::yield_now();
                action = returned;
            }
            Err(UserActionSendError::Saturated(returned)) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
                action = returned;
            }
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "host action channel closed ({error:?})"
                )));
            }
        }
    }
}

fn write_line(stdout: &mut dyn Write, line: &str) -> CliResult<()> {
    writeln!(stdout, "{line}").map_err(|_| CliFailure::new("stdout failed"))?;
    stdout.flush().map_err(|_| CliFailure::new("stdout failed"))
}

fn join_worker(worker: thread::JoinHandle<CliResult<()>>, deadline: Instant) -> CliResult<()> {
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            drop(worker);
            return Err(CliFailure::new(
                "plak agent host worker exceeded its shutdown deadline",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
    worker
        .join()
        .map_err(|_| CliFailure::new("plak agent host worker failed"))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn frames_are_bounded_before_newline_and_decode() {
        let mut endless = std::io::BufReader::with_capacity(7, std::io::repeat(b'x'));
        assert!(
            read_frame(&mut endless, 32)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        let mut input = std::io::Cursor::new("ééé\n{}\nlast");
        assert_eq!(read_frame(&mut input, 7).unwrap().as_deref(), Some("ééé\n"));
        assert_eq!(read_frame(&mut input, 7).unwrap().as_deref(), Some("{}\n"));
        assert_eq!(read_frame(&mut input, 7).unwrap().as_deref(), Some("last"));
        assert!(read_frame(&mut input, 7).unwrap().is_none());
        let mut input = std::io::Cursor::new("12345678\nvalid\n");
        assert!(read_frame(&mut input, 7).is_err());
        assert_eq!(
            input.position(),
            0,
            "overflow disconnects without unbounded draining"
        );
    }

    /// Every JSONL session ends with exactly one terminal record.
    ///
    /// 2026-09-18, live attempt 3: three Grok tree sessions ended after a
    /// `tree` record with no `bye`, no `error` and no error status. A client
    /// cannot tell that from the process being killed. The drive is now
    /// wrapped so that a panic terminates the stream before it unwinds, and a
    /// drive error emits its `error` object before the `bye`.
    #[test]
    fn a_completed_session_ends_with_exactly_one_bye() {
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let mut stdout = Vec::new();
        let outcome = run_agent_stdio_with_input(
            config,
            false,
            &mut stdout,
            &mut Vec::new(),
            std::io::Cursor::new(
                "{\"v\":1,\"kind\":\"prompt\",\"id\":\"p\",\"text\":\"run the loopback\"}\n",
            ),
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_terminated_exactly_once(&stdout);
    }

    /// A stop asks the host to interrupt before it asks it to quit.
    ///
    /// 2026-09-18, live: `SIGTERM` on the JSONL face exited after the full
    /// one-second deadline and left the branch head stale, so every signalled
    /// session failed to resume. The watcher had been installed with a counter
    /// this face never reads, so the graceful stop was never asked for at all.
    /// It is now the drive that notices the signal, because only the drive
    /// knows whether a turn is in flight, and it enters the very shutdown that
    /// end of input and a `quit` command enter. The order is the load-bearing
    /// part: a quit alone abandons an in-flight turn instead of interrupting
    /// it.
    #[test]
    fn a_terminating_signal_asks_the_drive_for_the_same_stop_quit_asks_for() {
        let (actions, received) = crate::surface_protocol::user_action_channel();
        begin_shutdown(&actions, true).expect("a fresh queue accepts both");
        let mut delivered = Vec::new();
        while let Ok(delivery) = received.try_recv() {
            delivered.push(delivery.into_parts().0);
        }
        assert!(
            matches!(
                delivered.as_slice(),
                [UserAction::Interrupt, UserAction::Quit]
            ),
            "{delivered:?}"
        );
    }

    /// A signalled stop says so, once, immediately before its one `bye`.
    ///
    /// The graceful teardown succeeds precisely because the stop was asked
    /// for, so no stage fails and nothing would otherwise distinguish a run cut
    /// short by a supervisor from one that finished. The exit status carries
    /// the same fact.
    #[test]
    fn a_signalled_stop_is_reported_before_the_terminal_record() {
        const SIGNALLED: fn() -> Option<&'static str> = || Some("stopped by a signal");
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let mut stdout = Vec::new();
        let outcome = run_agent_stdio_stopping_on(
            config,
            false,
            &mut stdout,
            &mut Vec::new(),
            std::io::Cursor::new(
                "{\"v\":1,\"kind\":\"prompt\",\"id\":\"p\",\"text\":\"run the loopback\"}\n",
            ),
            SIGNALLED,
            || 0,
        );
        assert!(
            matches!(outcome, Ok(ProcessOutcome::Interrupted)),
            "{outcome:?}"
        );
        let records = assert_terminated_exactly_once(&stdout);
        let reason = records
            .iter()
            .position(|record| record["kind"] == "error")
            .expect("a signalled stop must say why the stream ended");
        assert_eq!(reason, records.len().saturating_sub(2), "{records:?}");
        assert_eq!(
            records[reason]["message"].as_str().unwrap(),
            SIGNALLED().expect("the signal was recorded")
        );
    }

    /// A drive that genuinely fails emits its reason, then exactly one `bye`.
    ///
    /// An oversized stdin frame is a real drive error: `read_frame` refuses it
    /// and the reader hands the failure to the drive, which returns `Err`. The
    /// earlier version of this test fed `{"v":1,"kind":"nonsense"}`, which the
    /// command parser handles without failing anything, so it asserted nothing
    /// and passed before the change it was meant to cover.
    #[test]
    fn a_failed_drive_emits_an_error_then_exactly_one_bye() {
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let mut stdout = Vec::new();
        let oversized = format!("{}\n", "x".repeat(agent_jsonl::MAX_FRAME_BYTES + 1));
        let outcome = run_agent_stdio_with_input(
            config,
            false,
            &mut stdout,
            &mut Vec::new(),
            std::io::Cursor::new(oversized),
        );
        let error = outcome.expect_err("an oversized frame must fail the drive");
        let records = assert_terminated_exactly_once(&stdout);
        let reason = records
            .iter()
            .position(|record| record["kind"] == "error")
            .expect("a failed drive must say why it ended");
        assert_eq!(
            reason,
            records.len().saturating_sub(2),
            "the reason immediately precedes the terminal record: {records:?}"
        );
        assert_eq!(
            records[reason]["message"].as_str().unwrap(),
            error.message(),
            "the client reads the same reason the exit code carries"
        );
    }

    /// Input that sends `prompt`, then holds `tail` (and EOF) until the host
    /// has finished that root turn.
    ///
    /// Shutdown racing a running turn escalates once the cancel misses its
    /// bound, and an escalated quit abandons leases without hub retirement
    /// (spec 02, quit). Under load the scripted turn can miss that bound, so a
    /// test that asserts a joined child-retirement failure must quit idle.
    fn closing_after_the_root_terminal(
        config: &mut HostConfig<crate::surface_protocol::PlakExtension>,
        prompt: &'static str,
        tail: &'static str,
    ) -> impl BufRead + Send + 'static {
        struct AfterTerminal {
            terminal: Option<mpsc::Receiver<()>>,
            tail: std::io::Cursor<&'static [u8]>,
        }
        impl std::io::Read for AfterTerminal {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if let Some(terminal) = self.terminal.take() {
                    // A host that fails before its terminal drops the sender.
                    let _ = terminal.recv_timeout(JOIN_TIMEOUT);
                }
                std::io::Read::read(&mut self.tail, buf)
            }
        }
        let (terminal_ready, terminal) = mpsc::channel();
        std::sync::Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .observe = std::sync::Arc::new(move |workflow, boundary| {
            if workflow == plakous_hosted::runtime::TestHostWorkflow::RootDelivery
                && boundary == plakous_hosted::runtime::TestHostBoundary::TerminalReady
            {
                let _ = terminal_ready.send(());
            }
        });
        std::io::BufReader::new(std::io::Read::chain(
            std::io::Cursor::new(prompt.as_bytes()),
            AfterTerminal {
                terminal: Some(terminal),
                tail: std::io::Cursor::new(tail.as_bytes()),
            },
        ))
    }

    /// An error raised during teardown still terminates the stream.
    ///
    /// 2026-09-18, live attempt 4: three Grok tree rows exited 1 with the
    /// transcript stopping after a tree record, nothing killed and cleanup
    /// joined. An ordinary `Err` reaching main after the stream had started is
    /// the shape, so the terminal record has to be written before it, not
    /// after. Child shutdown is the teardown path a tree case actually
    /// exercises.
    #[test]
    fn a_teardown_failure_after_the_stream_started_still_writes_the_terminal_record() {
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let id = config.session.id.clone();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let _hub = executor
            .block_on(
                plakous_hosted::runtime::test_kit::install_failing_child_shutdown(
                    &id,
                    &config.session.workspace_root,
                ),
            )
            .unwrap();
        let input = closing_after_the_root_terminal(
            &mut config,
            "{\"v\":1,\"kind\":\"prompt\",\"id\":\"p\",\"text\":\"run the loopback\"}\n",
            "",
        );
        let mut stdout = Vec::new();
        let outcome =
            run_agent_stdio_with_input(config, false, &mut stdout, &mut Vec::new(), input);
        let error = outcome.expect_err("the injected child shutdown must fail the run");
        assert!(
            error.message().contains("injected child release refusal"),
            "{error:?}"
        );
        // Exit code unchanged by the terminal record.
        assert_eq!(error.exit_code(), 1);
        let records = assert_terminated_exactly_once(&stdout);
        assert!(
            records.iter().any(|record| record["kind"] == "error"),
            "a teardown failure must say why the stream ended: {records:?}"
        );
    }

    /// A child that never completes release is a bounded shutdown failure,
    /// not a quiet detach: the run fails with the retirement reason, and the
    /// stream still ends with exactly one `error` record and one `bye`.
    #[test]
    fn a_hung_teardown_reports_the_shutdown_bound_and_still_writes_the_terminal_record() {
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let id = config.session.id.clone();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let _hub = executor
            .block_on(
                plakous_hosted::runtime::test_kit::install_hanging_child_shutdown(
                    &id,
                    &config.session.workspace_root,
                ),
            )
            .unwrap();
        let input = closing_after_the_root_terminal(
            &mut config,
            "{\"v\":1,\"kind\":\"prompt\",\"id\":\"p\",\"text\":\"run the loopback\"}\n",
            "",
        );
        let mut stdout = Vec::new();
        let outcome =
            run_agent_stdio_with_input(config, false, &mut stdout, &mut Vec::new(), input);
        let error = outcome.expect_err("a hung child shutdown must fail the run");
        assert!(error.message().contains("child retirement"), "{error:?}");
        let records = assert_terminated_exactly_once(&stdout);
        let errors = records
            .iter()
            .filter(|record| record["kind"] == "error")
            .count();
        assert_eq!(errors, 1, "exactly one error record: {records:?}");
    }

    /// A reader that stalls does not truncate the stream.
    ///
    /// 2026-09-18, live attempt 4: three Grok tree rows ended with no `bye`,
    /// exit code 1, `timed_out` false, cleanup joined and nothing killed — a
    /// normal non-zero exit with a transcript that simply stops. A JSONL
    /// client that reads slowly fills the pipe; a non-blocking stdout then
    /// answers `WouldBlock`, or reports zero progress, and the old writer
    /// treated either as fatal. Because a poisoned writer also skips the
    /// terminal record, the transcript stopped mid-way with nothing saying
    /// why. Waiting for the reader is the right answer.
    #[test]
    fn a_stalled_reader_does_not_truncate_the_stream() {
        /// Answers `WouldBlock` and zero-progress a few times per write.
        struct Stalls {
            remaining_stalls: usize,
            buf: Vec<u8>,
        }
        impl Write for Stalls {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                if self.remaining_stalls > 0 {
                    self.remaining_stalls -= 1;
                    return if self.remaining_stalls.is_multiple_of(2) {
                        Ok(0)
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "pipe full",
                        ))
                    };
                }
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Stalls {
            remaining_stalls: 6,
            buf: Vec::new(),
        };
        {
            let mut output = Output {
                writer: &mut writer,
                failed: false,
            };
            write_line(&mut output, &bye_line(None)).expect("a stall is not a failure");
            assert!(
                !output.failed,
                "a stalled reader must not poison the stream"
            );
        }
        let text = String::from_utf8(writer.buf).unwrap();
        assert!(text.contains("\"kind\":\"bye\""), "{text}");
    }

    /// A genuinely broken pipe is still fatal, and still skips the record.
    #[test]
    fn a_broken_pipe_remains_fatal() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "reader is gone",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut broken = Broken;
        let mut output = Output {
            writer: &mut broken,
            failed: false,
        };
        assert!(write_line(&mut output, &bye_line(None)).is_err());
        assert!(
            output.failed,
            "a reader that is gone is not a stall; there is nothing to wait for"
        );
    }

    /// A panic in the drive terminates the stream before it unwinds.
    ///
    /// This is the shape of the live silent exit: output, then nothing, with no
    /// `bye` and no `error`. The panic still propagates, so the process's own
    /// failure disposition is unchanged; only the stream is closed properly
    /// first.
    #[test]
    fn a_panicking_drive_still_terminates_its_stream() {
        /// Panics on one chosen write, then behaves.
        struct PanicOnce {
            writes: usize,
            panic_at: usize,
            buf: Vec<u8>,
        }
        impl Write for PanicOnce {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.writes = self.writes.saturating_add(1);
                assert!(
                    self.writes != self.panic_at,
                    "injected drive panic on write {}",
                    self.writes
                );
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct Shared(std::sync::Arc<std::sync::Mutex<PanicOnce>>);
        impl Write for Shared {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .write(data)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let writer = std::sync::Arc::new(std::sync::Mutex::new(PanicOnce {
            writes: 0,
            // The handshake lands first; panic on the write after it.
            panic_at: 2,
            buf: Vec::new(),
        }));
        let held = std::sync::Arc::clone(&writer);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _ = run_agent_stdio_with_input(
                config,
                false,
                &mut Shared(held),
                &mut Vec::new(),
                std::io::Cursor::new(
                    "{\"v\":1,\"kind\":\"prompt\",\"id\":\"p\",\"text\":\"run the loopback\"}\n",
                ),
            );
        }));
        assert!(
            panicked.is_err(),
            "the panic must still propagate; only the stream is closed first"
        );
        let emitted = {
            let guard = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.buf.clone()
        };
        let text = String::from_utf8(emitted).unwrap();
        let byes = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["kind"] == "bye")
            .count();
        assert_eq!(
            byes, 1,
            "a panicking drive still terminates its stream: {text}"
        );
    }

    /// Parses the emitted stream and asserts it ends with exactly one `bye`.
    fn assert_terminated_exactly_once(stdout: &[u8]) -> Vec<serde_json::Value> {
        let text = String::from_utf8(stdout.to_vec()).unwrap();
        let records: Vec<serde_json::Value> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!records.is_empty(), "the stream emitted nothing");
        let byes = records
            .iter()
            .filter(|record| record["kind"] == "bye")
            .count();
        assert_eq!(byes, 1, "exactly one terminal record: {records:?}");
        assert_eq!(
            records.last().unwrap()["kind"],
            "bye",
            "the terminal record is last: {records:?}"
        );
        records
    }

    #[test]
    fn worker_join_uses_the_supplied_deadline() {
        let (release, held) = mpsc::channel();
        let worker = thread::spawn(move || {
            held.recv().unwrap();
            Ok(())
        });
        let error = join_worker(worker, Instant::now()).unwrap_err();
        assert!(error.message().contains("shutdown deadline"));
        release.send(()).unwrap();
    }

    #[test]
    fn handshake_failure_still_joins_and_preserves_release_failure() {
        struct FailedWriter;
        impl Write for FailedWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected output failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let store = config.store.clone();
        let id = config.session.id.clone();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let _hub = executor
            .block_on(
                plakous_hosted::runtime::test_kit::install_failing_child_shutdown(
                    &id,
                    &config.session.workspace_root,
                ),
            )
            .unwrap();
        let error = run_agent_stdio_with_input(
            config,
            false,
            &mut FailedWriter,
            &mut Vec::new(),
            std::io::Cursor::new(Vec::<u8>::new()),
        )
        .unwrap_err();
        assert!(error.message().contains("stdout failed"), "{error:?}");
        assert!(
            error.message().contains("injected child release refusal"),
            "{error:?}"
        );
        assert!(
            store.load(&id).is_err(),
            "handshake failure admits no root turn"
        );
    }

    #[test]
    fn input_failure_still_joins_and_writes_bye() {
        struct FailedRead;
        impl std::io::Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected input failure"))
            }
        }
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let _hub = executor
            .block_on(
                plakous_hosted::runtime::test_kit::install_failing_child_shutdown(
                    &config.session.id,
                    &config.session.workspace_root,
                ),
            )
            .unwrap();
        let mut stdout = Vec::new();
        let error = run_agent_stdio_with_input(
            config,
            false,
            &mut stdout,
            &mut Vec::new(),
            std::io::BufReader::new(FailedRead),
        )
        .unwrap_err();
        assert!(
            error.message().contains("injected input failure"),
            "{error:?}"
        );
        assert!(
            error.message().contains("injected child release refusal"),
            "{error:?}"
        );
        assert!(
            std::str::from_utf8(&stdout)
                .unwrap()
                .contains("\"kind\":\"bye\"")
        );
    }

    #[test]
    fn admitted_output_failure_joins_and_publishes_parent_locator() {
        struct FailBusy(Vec<u8>, mpsc::Sender<()>);
        impl Write for FailBusy {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.extend_from_slice(bytes);
                if self
                    .0
                    .windows(b"\"busy\":true".len())
                    .any(|part| part == b"\"busy\":true")
                {
                    self.1.send(()).unwrap();
                    Err(std::io::Error::other("injected admitted output failure"))
                } else {
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::GatedEffect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let store = config.store.clone();
        let id = config.session.id.clone();
        let (output_failed, failure_observed) = mpsc::channel();
        let (entered, at_terminal) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let held = std::sync::Mutex::new(held);
        std::sync::Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .observe = std::sync::Arc::new(move |_, boundary| {
            if boundary == plakous_hosted::runtime::TestHostBoundary::TerminalReady {
                entered.send(()).unwrap();
                held.lock().unwrap().recv().unwrap();
            }
        });
        let command = thread::spawn(move || {
            run_agent_stdio_with_input(
                config,
                false,
                &mut FailBusy(Vec::new(), output_failed),
                &mut Vec::new(),
                std::io::Cursor::new(
                    "{\"v\":1,\"kind\":\"prompt\",\"id\":\"p\",\"text\":\"offline effect\"}\n",
                ),
            )
        });
        // Startup is not part of the shutdown interval. Observe the actual
        // transport failure first, then allow only the owner's cleanup bound.
        failure_observed.recv_timeout(JOIN_TIMEOUT).unwrap();
        at_terminal.recv_timeout(JOIN_TIMEOUT).unwrap();
        assert!(
            !command.is_finished(),
            "output failure returned before parent publication"
        );
        release.send(()).unwrap();
        let error = command.join().unwrap().unwrap_err();
        assert!(error.message().contains("stdout failed"), "{error:?}");
        assert!(store.load(&id).unwrap().plakous_locator.is_some());
    }

    #[test]
    fn openrouter_hello_preserves_canonical_effort_and_omission() {
        const CHILD: &str = "PLAK_TEST_OPENROUTER_HELLO";
        if std::env::var_os(CHILD).is_none() {
            let isolated = crate::home::paths::PlakPaths::isolated_tempdir();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "agent_stdio::tests::openrouter_hello_preserves_canonical_effort_and_omission",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("XDG_STATE_HOME", isolated.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        for canonical_effort in [Some("low"), None] {
            let fixture = crate::observation_parity_tests::Fixture::new(
                crate::observation_parity_tests::Script::Effect,
            );
            let injected = fixture.host_config();
            let paths = crate::home::paths::PlakPaths::from_home(injected.product_home.clone());
            let mut session = injected.session.clone();
            session.account = Some("openrouter".into());
            session.model = Some("z-ai/glm-5.3-flash".into());
            session.effort = canonical_effort.map(str::to_owned);
            session.composition = crate::product_config::ProductCompositionSelection::Direct;
            session.target_selection = crate::product_config::ProductTargetSelection::OpenRouter {
                wire: crate::product_config::ProductWireBinding::resolve(
                    crate::product_config::ProductProtocolSelection::Auto,
                ),
                routing: crate::product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
            };
            let mut config = crate::commands::host_config::build_host_config(
                &paths,
                &injected.state_roots,
                session,
                Some("openrouter"),
                Some("z-ai/glm-5.3-flash"),
                plakous::agent::ReasoningEffort::High,
                true,
                &injected.cwd,
                &injected.ready_catalog,
                None,
                &[],
            )
            .unwrap();
            assert_eq!(config.effort.map(effort_label).as_deref(), canonical_effort);
            assert_eq!(config.effort_explicit, canonical_effort.is_some());
            // Keep the real product binding and transport; replace only the
            // provider execution with the established offline host injection.
            config.target = injected.target;
            config.test_injection = injected.test_injection;
            config.topology_enabled = false;
            config.compaction_enabled = false;
            let mut stdout = Vec::new();
            run_agent_stdio_with_input(
                config,
                false,
                &mut stdout,
                &mut Vec::new(),
                std::io::Cursor::new("{\"v\":1,\"kind\":\"quit\",\"id\":\"q\"}\n"),
            )
            .unwrap();
            let hello = std::str::from_utf8(&stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .find(|event| event["kind"] == "hello")
                .unwrap();
            assert_eq!(hello["account"], "openrouter");
            assert_eq!(hello["effort"].as_str(), canonical_effort);
            assert_eq!(hello.get("effort").is_some(), canonical_effort.is_some());
        }
    }

    #[test]
    fn saturated_controls_keep_the_shutdown_lane_available() {
        let (actions, _receiver) = crate::surface_protocol::user_action_channel();
        while actions.send(UserAction::Prompt("queued".into())).is_ok() {}
        let error = send_action_until(
            &actions,
            UserAction::Prompt("overflow".into()),
            Instant::now(),
        )
        .unwrap_err();
        assert!(error.message().contains("Saturated"));
        assert!(begin_shutdown_until(&actions, true, Instant::now()).is_ok());
    }

    #[test]
    fn jsonl_returns_child_shutdown_failure_after_publishing_parent_locator() {
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let store = config.store.clone();
        let id = config.session.id.clone();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let _hub = executor
            .block_on(
                plakous_hosted::runtime::test_kit::install_failing_child_shutdown(
                    &id,
                    &config.session.workspace_root,
                ),
            )
            .unwrap();
        let input = closing_after_the_root_terminal(
            &mut config,
            "{\"v\":1,\"kind\":\"prompt\",\"id\":\"p\",\"text\":\"offline release regression\"}\n",
            "{\"v\":1,\"kind\":\"quit\",\"id\":\"q\"}\n",
        );
        let mut stdout = Vec::new();
        let error = run_agent_stdio_with_input(config, false, &mut stdout, &mut Vec::new(), input)
            .unwrap_err();
        assert!(
            error.message().contains("injected child release refusal"),
            "{error:?}"
        );
        assert!(store.load(&id).unwrap().plakous_locator.is_some());
        assert!(
            std::str::from_utf8(&stdout)
                .unwrap()
                .contains("\"kind\":\"bye\"")
        );
    }

    #[test]
    fn jsonl_command_refusal_does_not_become_a_terminal_worker_failure() {
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = false;
        let mut stdout = Vec::new();
        let outcome = run_agent_stdio_with_input(
            config,
            false,
            &mut stdout,
            &mut Vec::new(),
            std::io::Cursor::new(concat!(
                "{\"v\":1,\"kind\":\"set_model\",\"id\":\"bad\",\"account\":\"codex\",\"model\":\"missing\",\"effort\":\"medium\"}\n",
                "{\"v\":1,\"kind\":\"quit\",\"id\":\"q\"}\n",
            )),
        )
        .unwrap();
        assert_eq!(outcome, ProcessOutcome::Success);
        let output = std::str::from_utf8(&stdout).unwrap();
        assert!(output.contains("\"kind\":\"error\""), "{output}");
        assert!(
            output.contains("unknown JSONL kind `set_model`"),
            "{output}"
        );
        assert!(output.contains("\"kind\":\"bye\""), "{output}");
    }

    #[test]
    fn actual_jsonl_idle_invocation_keeps_its_command_id() {
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let mut config = fixture.host_config();
        config.topology_enabled = true;
        let _hub = plakous_hosted::runtime::test_kit::install_idle_topology_hub(
            &config.session.id,
            &config.session.workspace_root,
        );
        let mut stdout = Vec::new();
        let outcome = run_agent_stdio_with_input(
            config, false, &mut stdout, &mut Vec::new(),
            std::io::Cursor::new(concat!(
                "{\"v\":1,\"kind\":\"invoke_tool\",\"id\":\"iv\",\"name\":\"list_agents\",\"arguments\":{}}\n",
                "{\"v\":1,\"kind\":\"quit\",\"id\":\"q\"}\n",
            )),
        ).unwrap();
        assert_eq!(outcome, ProcessOutcome::Success);
        let output = String::from_utf8(stdout).unwrap();
        let tools: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["kind"] == "tool")
            .collect();
        assert_eq!(tools.len(), 2, "{output}");
        assert!(tools.iter().all(|event| event["id"] == "iv"), "{output}");
        assert_eq!(tools[1]["state"], "done", "{output}");
    }

    #[test]
    fn attach_actions_resume_then_navigator() {
        assert_eq!(
            actions_on_successful_attach(true, true, true),
            vec![
                UserAction::ResumeTopologyTree,
                UserAction::OpenTopologyNavigator,
            ]
        );
        assert_eq!(
            actions_on_successful_attach(false, true, true),
            vec![UserAction::OpenTopologyNavigator]
        );
        assert!(actions_on_successful_attach(false, false, true).is_empty());
        assert!(actions_on_successful_attach(false, true, false).is_empty());
    }

    #[test]
    fn hello_gate_refuses_paused_without_resume_tree() {
        assert_eq!(hello_gate(false, true, false), HelloGate::RefusePaused);
        assert_eq!(hello_gate(true, true, false), HelloGate::Wait);
        assert_eq!(hello_gate(true, true, true), HelloGate::RefusePaused);
        assert_eq!(hello_gate(false, false, true), HelloGate::Hello);
        assert_eq!(hello_gate(true, false, true), HelloGate::Hello);
        // resume requested, hub not registered yet is treated as paused Wait
        assert_eq!(hello_gate(true, true, false), HelloGate::Wait);
    }

    #[test]
    fn observations_buffer_until_hello() {
        let mut pending = Vec::new();
        emit_lines(
            false,
            &mut pending,
            &mut Vec::new(),
            vec!["{\"kind\":\"status\"}".into()],
        )
        .unwrap();
        assert_eq!(pending.len(), 1);
        let mut out = Vec::new();
        emit_lines(
            true,
            &mut pending,
            &mut out,
            vec!["{\"kind\":\"tool\"}".into()],
        )
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            std::str::from_utf8(&out).unwrap().trim(),
            "{\"kind\":\"tool\"}"
        );
    }

    #[test]
    fn prompt_id_preserved_when_tree_command_arrives_mid_turn() {
        let mut busy = true;
        let mut turn_busy = true;
        let mut tree_state = "live".to_owned();
        let mut prompt_id = Some("prompt-1".to_owned());
        let command_id = Some("tree-1".to_owned());
        let mut pending_tree_snapshot_id = Some("tree-1".to_owned());

        let assistant_lines = handle_event(
            &HostEvent::AssistantDelta("partial".into()),
            &mut busy,
            &mut turn_busy,
            &mut tree_state,
            &mut prompt_id,
            command_id.as_ref(),
            &mut pending_tree_snapshot_id,
        );
        assert_eq!(assistant_lines.len(), 1);
        assert!(assistant_lines[0].contains("\"id\":\"prompt-1\""));

        pending_tree_snapshot_id = None;
        let tool_lines = handle_event(
            &HostEvent::ToolLine {
                parent_call_id: None,
                cell_id: None,
                kind: plakous_host_protocol::views::presentation::ActivityKind::Tool,
                call_id: None,
                name: "read".into(),
                state: "done".into(),
                subject: None,
                detail: None,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            },
            &mut busy,
            &mut turn_busy,
            &mut tree_state,
            &mut prompt_id,
            command_id.as_ref(),
            &mut pending_tree_snapshot_id,
        );
        assert_eq!(tool_lines.len(), 1);
        assert!(tool_lines[0].contains("\"id\":\"prompt-1\""));
    }

    #[test]
    fn unsolicited_tree_snapshot_omits_id_while_prompt_in_flight() {
        use plakous_host_protocol::topology::{NodeStatus, TreeAdmissionState, WorkspaceMode};
        use plakous_host_protocol::views::{NodeView, TreeView};

        let mut busy = true;
        let mut turn_busy = true;
        let mut tree_state = "live".to_owned();
        let mut prompt_id = Some("prompt-1".to_owned());
        let command_id = Some("tree-1".to_owned());
        let mut pending_tree_snapshot_id = None;

        let lines = handle_event(
            &HostEvent::TopologyNavigatorSnapshot(TreeView {
                tree_id: "tree-1".into(),
                revision: 4,
                state: TreeAdmissionState::Live,
                solicited: false,
                nodes: vec![NodeView {
                    agent_id: "tree-1".into(),
                    parent_agent_id: None,
                    depth: 0,
                    status: NodeStatus::Running,
                    attempt_status: None,
                    attempt_seq: None,
                    attempt_id: None,
                    attempts: Vec::new(),
                    tier: None,
                    description: "root".into(),
                    prompt: "root".into(),
                    workspace_path: "/tmp".into(),
                    workspace_mode: WorkspaceMode::Inherit,
                    workspace_status: None,
                    candidate_id: None,
                    blocked: None,
                    usage: None,
                    active_turn: true,
                    live_progress: None,
                    unread_mail: 0,
                    has_session: true,
                    template: None,
                    model_account: "grok".into(),
                    model_tuple: "grok-4.6".into(),
                    model_effort: None,
                    turns: Some(1),
                    started_at_unix: None,
                    ended_at_unix: None,
                    fault: None,
                    waiting_on: None,
                }],
            }),
            &mut busy,
            &mut turn_busy,
            &mut tree_state,
            &mut prompt_id,
            command_id.as_ref(),
            &mut pending_tree_snapshot_id,
        );
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("\"id\":"));
    }

    #[test]
    fn explicit_tree_command_snapshot_uses_tree_command_id() {
        use plakous_host_protocol::topology::{NodeStatus, TreeAdmissionState, WorkspaceMode};
        use plakous_host_protocol::views::{NodeView, TreeView};

        let mut busy = false;
        let mut turn_busy = false;
        let mut tree_state = "live".to_owned();
        let mut prompt_id = None;
        let command_id = Some("tree-2".to_owned());
        let mut pending_tree_snapshot_id = Some("tree-2".to_owned());

        let lines = handle_event(
            &HostEvent::TopologyNavigatorSnapshot(TreeView {
                tree_id: "tree-1".into(),
                revision: 5,
                state: TreeAdmissionState::Live,
                solicited: true,
                nodes: vec![NodeView {
                    agent_id: "tree-1".into(),
                    parent_agent_id: None,
                    depth: 0,
                    status: NodeStatus::Running,
                    attempt_status: None,
                    attempt_seq: None,
                    attempt_id: None,
                    attempts: Vec::new(),
                    tier: None,
                    description: "root".into(),
                    prompt: "root".into(),
                    workspace_path: "/tmp".into(),
                    workspace_mode: WorkspaceMode::Inherit,
                    workspace_status: None,
                    candidate_id: None,
                    blocked: None,
                    usage: None,
                    active_turn: false,
                    live_progress: None,
                    unread_mail: 0,
                    has_session: true,
                    template: None,
                    model_account: "grok".into(),
                    model_tuple: "grok-4.6".into(),
                    model_effort: None,
                    turns: Some(1),
                    started_at_unix: None,
                    ended_at_unix: None,
                    fault: None,
                    waiting_on: None,
                }],
            }),
            &mut busy,
            &mut turn_busy,
            &mut tree_state,
            &mut prompt_id,
            command_id.as_ref(),
            &mut pending_tree_snapshot_id,
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"id\":\"tree-2\""));
        assert!(pending_tree_snapshot_id.is_none());
    }

    #[test]
    fn unsolicited_snapshot_does_not_consume_pending_tree_id() {
        use plakous_host_protocol::topology::{NodeStatus, TreeAdmissionState, WorkspaceMode};
        use plakous_host_protocol::views::{NodeView, TreeView};

        let mut busy = false;
        let mut turn_busy = false;
        let mut tree_state = "live".to_owned();
        let mut prompt_id = None;
        let command_id = Some("tree-2".to_owned());
        let mut pending_tree_snapshot_id = Some("tree-2".to_owned());

        let lines = handle_event(
            &HostEvent::TopologyNavigatorSnapshot(TreeView {
                tree_id: "tree-1".into(),
                revision: 6,
                state: TreeAdmissionState::Live,
                solicited: false,
                nodes: vec![NodeView {
                    agent_id: "tree-1".into(),
                    parent_agent_id: None,
                    depth: 0,
                    status: NodeStatus::Running,
                    attempt_status: None,
                    attempt_seq: None,
                    attempt_id: None,
                    attempts: Vec::new(),
                    tier: None,
                    description: "root".into(),
                    prompt: "root".into(),
                    workspace_path: "/tmp".into(),
                    workspace_mode: WorkspaceMode::Inherit,
                    workspace_status: None,
                    candidate_id: None,
                    blocked: None,
                    usage: None,
                    active_turn: false,
                    live_progress: None,
                    unread_mail: 0,
                    has_session: true,
                    template: None,
                    model_account: "grok".into(),
                    model_tuple: "grok-4.6".into(),
                    model_effort: None,
                    turns: Some(1),
                    started_at_unix: None,
                    ended_at_unix: None,
                    fault: None,
                    waiting_on: None,
                }],
            }),
            &mut busy,
            &mut turn_busy,
            &mut tree_state,
            &mut prompt_id,
            command_id.as_ref(),
            &mut pending_tree_snapshot_id,
        );
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("\"id\":"));
        assert_eq!(pending_tree_snapshot_id.as_deref(), Some("tree-2"));
    }

    #[test]
    fn set_busy_false_clears_prompt_id_without_synthetic_turn_idle() {
        let mut busy = true;
        let mut turn_busy = true;
        let mut tree_state = "live".to_owned();
        let mut prompt_id = Some("prompt-1".to_owned());
        let command_id = Some("prompt-1".to_owned());
        let mut pending_tree_snapshot_id = None;

        let lines = handle_event(
            &HostEvent::SetBusy(false),
            &mut busy,
            &mut turn_busy,
            &mut tree_state,
            &mut prompt_id,
            command_id.as_ref(),
            &mut pending_tree_snapshot_id,
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"kind\":\"busy\""));
        assert!(lines[0].contains("\"busy\":false"));
        assert!(!lines[0].contains("\"outcome\":\"idle\""));
        assert!(prompt_id.is_none());
    }
    #[test]
    fn restoration_and_runtime_rejection_keep_later_identical_prompt_origin() {
        let mut origins = PromptOrigins::default();
        let mut active = None;
        let action = UserAction::Prompt("same".into());
        origins.submitted(&action, "p1".into());
        origins.observe(&HostEvent::IntentStarted(Some("p1".into())), &mut active);
        let item = plakous_host_protocol::QueueItem {
            rejected: false,
            id: "q1".into(),
            revision: 1,
            text: "same".into(),
        };
        origins.observe(&HostEvent::PromptAdmitted(item.clone()), &mut active);
        origins.observe(&HostEvent::PromptStarted(Some(item)), &mut active);
        origins.submitted(&action, "p2".into());
        origins.observe(&HostEvent::DraftRestored("same".into()), &mut active);
        origins.observe(&HostEvent::DraftRejected(action), &mut active);
        assert_eq!(origins.pending.front().unwrap().1, "p2");
        assert_eq!(active.as_deref(), Some("p1"));
    }

    #[test]
    fn queued_origins_retire_on_edit_remove_clear_or_rejection() {
        use plakous_host_protocol::{QueueDrainState, QueueItem};
        for retirement in ["remove", "edit", "clear", "reject"] {
            let mut origins = PromptOrigins::default();
            let mut active = Some("p0".to_owned());
            let action = UserAction::FollowUp("same".into());
            let row = QueueItem {
                rejected: false,
                id: "queue1".into(),
                revision: 1,
                text: "same".into(),
            };
            origins.submitted(&action, "p1".into());
            if retirement == "reject" {
                origins.observe(&HostEvent::IntentStarted(Some("p1".into())), &mut active);
                origins.observe(&HostEvent::DraftRejected(action), &mut active);
            } else {
                origins.observe(&HostEvent::PromptAdmitted(row.clone()), &mut active);
                assert_eq!(active.as_deref(), Some("p0"));
                let event = match retirement {
                    "remove" => HostEvent::QueueSnapshot {
                        items: vec![],
                        drain: QueueDrainState::RestoredPaused,
                    },
                    "edit" => HostEvent::QueueSnapshot {
                        items: vec![QueueItem {
                            revision: 2,
                            ..row.clone()
                        }],
                        drain: QueueDrainState::RestoredPaused,
                    },
                    _ => HostEvent::ClearView,
                };
                origins.observe(&event, &mut active);
            }
            assert!(origins.pending.is_empty() && origins.admitted.is_empty());
            origins.observe(&HostEvent::PromptStarted(Some(row)), &mut active);
            assert!(active.is_none());
        }
    }

    #[test]
    fn real_mailbox_coalescing_keeps_same_text_queue_origins_exact() {
        use plakous_host_protocol::{QueueDrainState, QueueItem, observation_channel};
        let (tx, rx) = observation_channel();
        let mut origins = PromptOrigins::default();
        let mut active = Some("p0".to_owned());
        let row = |id: &str| QueueItem {
            rejected: false,
            id: id.into(),
            revision: 1,
            text: "same".into(),
        };
        for id in ["p1", "p2"] {
            origins.submitted(&UserAction::FollowUp("same".into()), id.into());
            tx.send(HostEvent::QueueSnapshot {
                items: if id == "p1" {
                    vec![row("restored"), row("p1")]
                } else {
                    vec![row("restored"), row("p1"), row("p2")]
                },
                drain: QueueDrainState::LiveAutoDrain,
            })
            .unwrap();
            tx.send(HostEvent::PromptAdmitted(row(id))).unwrap();
        }
        tx.send(HostEvent::SetBusy(false)).unwrap();
        tx.send(HostEvent::PromptStarted(Some(row("restored"))))
            .unwrap();
        tx.send(HostEvent::SetBusy(true)).unwrap();
        while let Ok(event) = rx.try_recv() {
            origins.observe(&event, &mut active);
        }
        assert!(active.is_none());
        for id in ["p1", "p2"] {
            tx.send(HostEvent::SetBusy(false)).unwrap();
            tx.send(HostEvent::PromptStarted(Some(row(id)))).unwrap();
            tx.send(HostEvent::SetBusy(true)).unwrap();
            while let Ok(event) = rx.try_recv() {
                origins.observe(&event, &mut active);
            }
            assert_eq!(active.as_deref(), Some(id));
        }
    }

    #[test]
    fn provenance_gap_disables_inference_even_when_old_receipts_follow_new_stdin() {
        use plakous_host_protocol::QueueItem;
        let mut origins = PromptOrigins::default();
        let mut active = Some("p0".to_owned());
        origins.submitted(&UserAction::FollowUp("same".into()), "p1".into());
        origins.observe(&HostEvent::PromptCorrelationReset, &mut active);
        origins.submitted(&UserAction::FollowUp("same".into()), "p2".into());
        let row = QueueItem {
            rejected: false,
            id: "old".into(),
            revision: 1,
            text: "same".into(),
        };
        origins.observe(&HostEvent::PromptAdmitted(row.clone()), &mut active);
        origins.observe(&HostEvent::PromptStarted(Some(row)), &mut active);
        assert!(active.is_none());
        assert!(origins.disabled);
    }

    #[test]
    fn saturated_mailbox_cannot_attribute_unknown_refusal_to_previous_turn() {
        use plakous_host_protocol::{limits::OBSERVATION_QUEUE_CAPACITY, observation_channel};
        let (tx, rx) = observation_channel();
        let mut origins = PromptOrigins::default();
        let mut active = Some("p0".to_owned());
        for _ in 0..OBSERVATION_QUEUE_CAPACITY {
            tx.send(HostEvent::DraftAccepted(UserAction::Prompt("fill".into())))
                .unwrap();
        }
        assert!(tx.send(HostEvent::PromptStarted(None)).is_err());
        tx.send(HostEvent::ModelRefusal { detail: None }).unwrap();
        let mut busy = false;
        let mut turn_busy = false;
        let mut tree = "live".to_owned();
        let command = Some("p1".to_owned());
        let mut saw_refusal = false;
        while let Ok(event) = rx.try_recv() {
            origins.observe(&event, &mut active);
            let lines = handle_event(
                &event,
                &mut busy,
                &mut turn_busy,
                &mut tree,
                &mut active,
                command.as_ref(),
                &mut None,
            );
            if matches!(event, HostEvent::ModelRefusal { .. }) {
                saw_refusal = true;
                assert!(!lines[0].contains("\"id\":"));
            }
        }
        assert!(saw_refusal && origins.disabled);
        let lines = handle_event(
            &HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::error(
                    plakous_host_protocol::views::transcript::NoticeScope::Host,
                    "command refused",
                ),
            ),
            &mut busy,
            &mut turn_busy,
            &mut tree,
            &mut active,
            command.as_ref(),
            &mut None,
        );
        assert!(lines[0].contains("\"id\":\"p1\""));
    }

    #[cfg(unix)]
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one actual stdio session spans gated admission, refusal and persisted queue checks"
    )]
    fn actual_jsonl_refusal_pauses_followup_until_resume_queue() {
        use plakous_offline_crash_harness::BoundaryController;
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use std::os::unix::net::UnixStream;
        struct Lines {
            pending: Vec<u8>,
            tx: mpsc::Sender<serde_json::Value>,
        }
        impl Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.pending.extend_from_slice(bytes);
                while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                    let row: Vec<_> = self.pending.drain(..=end).collect();
                    let value = serde_json::from_slice(&row).map_err(std::io::Error::other)?;
                    self.tx.send(value).map_err(std::io::Error::other)?;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let fixture = crate::observation_parity_tests::Fixture::new(
            crate::observation_parity_tests::Script::Effect,
        );
        let workspace = fixture.host_config().cwd;
        let release = workspace.join("release-stdio-refusal");
        let evidence = workspace.join("stdio-refusal-events.jsonl");
        let controller = BoundaryController::quiet_gated_from_evidence_path(
            CrashBoundary::ProviderDispatchOne,
            &evidence,
            &release,
        )
        .unwrap();
        let mut config = crate::observation_parity_tests::refusal_config_with_controller(
            &fixture, "", controller,
        );
        config.topology_enabled = false;
        let store = config.store.clone();
        let session = config.session.id.clone();
        let (mut input, reader) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_agent_stdio_with_input(
                config,
                false,
                &mut Lines {
                    pending: vec![],
                    tx,
                },
                &mut Vec::new(),
                std::io::BufReader::new(reader),
            )
        });
        let until = |predicate: &dyn Fn(&serde_json::Value) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let event = rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                if predicate(&event) {
                    return event;
                }
            }
        };
        until(&|event| event["kind"] == "ready");
        writeln!(
            input,
            "{{\"v\":1,\"kind\":\"prompt\",\"id\":\"p0\",\"text\":\"first refusal\"}}"
        )
        .unwrap();
        let busy = until(&|event| event["kind"] == "busy" && event["busy"] == true);
        assert_eq!(busy["id"], "p0");
        writeln!(
            input,
            "{{\"v\":1,\"kind\":\"prompt\",\"id\":\"p1\",\"text\":\"queued work\"}}"
        )
        .unwrap();
        until(&|event| {
            event["kind"] == "queue"
                && event["items"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["text"] == "queued work"))
        });
        std::fs::write(&release, "release").unwrap();
        let mut saw_refusal = false;
        let mut saw_usage = false;
        loop {
            let event = until(&|event| {
                event["kind"] == "model_refusal"
                    || event["kind"] == "turn"
                    || (event["kind"] == "busy" && event["busy"] == false)
            });
            assert_eq!(event["id"], "p0");
            match event["kind"].as_str().unwrap() {
                "model_refusal" => saw_refusal = true,
                "turn" => saw_usage = true,
                _ => break,
            }
        }
        assert!(saw_refusal && saw_usage);
        until(&|event| event["kind"] == "queue" && event["drain"] == "restored_paused");
        let record = store.load(&session).unwrap();
        assert_eq!(
            record.queue_drain_state,
            plakous_hosted::sessions::QueueDrainState::RestoredPaused
        );
        assert_eq!(record.pending_queue.len(), 1);
        let dispatches = plakous_offline_crash_harness::evidence::read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|row| {
                row.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count();
        assert_eq!(dispatches, 1);
        writeln!(input, "{{\"v\":1,\"kind\":\"resume_queue\",\"id\":\"rq\"}}").unwrap();
        until(&|event| event["kind"] == "queue" && event["drain"] == "live_auto_drain");
        let resumed = until(&|event| event["kind"] == "model_refusal");
        assert_eq!(resumed["id"], "p1");
        until(&|event| event["kind"] == "busy" && event["busy"] == false);
        let resumed_dispatches = plakous_offline_crash_harness::evidence::read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|row| {
                row.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count();
        assert_eq!(resumed_dispatches, 2);
        writeln!(input, "{{\"v\":1,\"kind\":\"quit\",\"id\":\"q\"}}").unwrap();
        until(&|event| event["kind"] == "bye");
        drop(input);
        assert_eq!(worker.join().unwrap().unwrap(), ProcessOutcome::Success);
    }
}
