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
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use plakous_hosted::runtime::{HostConfig, effort_label, spawn_host_worker};
use plakous_hosted::topology_runtime;

const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_POLL: Duration = Duration::from_millis(20);
const ACTION_RETRY: Duration = Duration::from_secs(2);
const IDLE_STEER_MESSAGE: &str = "steer is only valid while a turn is busy";

/// Attach the product host and drive it from JSONL stdin until quit or EOF.
pub fn run_agent_stdio(
    config: HostConfig<crate::surface_protocol::PlakExtension>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let outcome = run_agent_stdio_with_input(
        config,
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
        let _ = writeln!(
            stdout,
            "{}",
            plakous_host_protocol::envelope::coded_diagnostic_line(
                None,
                failure.code(),
                failure.message()
            )
        );
        let _ = writeln!(stdout, "{}", bye_line(None));
        let _ = stdout.flush();
    }
    outcome
}

fn run_agent_stdio_with_input(
    config: HostConfig<crate::surface_protocol::PlakExtension>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    input: impl BufRead + Send + 'static,
) -> CliResult<ProcessOutcome> {
    run_agent_stdio_stopping_on(
        config,
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
#[allow(
    clippy::too_many_lines,
    reason = "The stdio drive keeps input, host events, interrupts, and its single terminal emission in one ordered select loop."
)]
fn run_agent_stdio_stopping_on(
    mut config: HostConfig<crate::surface_protocol::PlakExtension>,
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
    let (actions, events, worker) = crate::surface_protocol::face_of(spawn_host_worker(config)?);
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
    let mut pending_stdout: Vec<String> = Vec::new();

    let mut navigator_opened = false;
    let mut pending_command: Option<(
        Instant,
        plakous_hosted::product_observation::JsonlCommandKind,
    )> = None;

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
                                Some(&session_id),
                            )
                            .into_iter()
                            .collect()
                        } else {
                            handle_event(
                                &event,
                                &mut JsonlSession {
                                    busy: &mut busy,
                                    turn_busy: &mut turn_busy,
                                    tree_state: &mut tree_state,
                                    prompt_id: &mut prompt_id,
                                    command_id: command_id.as_ref(),
                                    pending_tree_snapshot_id: &mut pending_tree_snapshot_id,
                                    session_id: Some(&session_id),
                                },
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
                            let need_navigator = !navigator_opened;
                            // A home may enable orchestration before this
                            // session has ever created a tree. The idle host
                            // pushes its first tree revision if one appears.
                            let hub_attached = topology_runtime::lookup(&tree_id).is_some();
                            for action in actions_on_successful_attach(
                                need_navigator,
                                topology_enabled && hub_attached,
                            ) {
                                send_action(&actions, action)?;
                            }
                            if need_navigator && topology_enabled && hub_attached {
                                navigator_opened = true;
                            }
                        }
                        if matches!(event, HostEvent::AgentAttached(false)) {
                            attached = true;
                            shutting_down = true;
                            shutdown_started = Instant::now();
                            begin_shutdown(&actions, busy)?;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if attached && !hello_sent {
                    if tree_state == "unknown" {
                        tree_state = "live".to_owned();
                    }
                    let (javascript_host, javascript_host_target) =
                        crate::sidecars::js_host::hello_status();
                    let (python_host, python_host_target) =
                        crate::sidecars::python_bundle::python_host_hello_status();
                    let python_available = crate::build_features::require_feature("python").is_ok();
                    let elapsed_ms =
                        u64::try_from(process_started.elapsed().as_millis()).unwrap_or(u64::MAX);
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
                            python_available.then_some(python_host.as_str()),
                            python_available.then_some(python_host_target),
                            Some(&crate::build_features::compiled_features()),
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
                            if matches!(command, AgentCommand::Tree { .. }) {
                                pending_tree_snapshot_id = Some(cmd_id.clone());
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
    let escalated_quit = matches!(
        &join_result,
        Ok(plakous_hosted::runtime::HostQuitOutcome::Escalated)
    );
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
                &plakous_host_protocol::envelope::coded_diagnostic_line(
                    command_id.as_deref(),
                    error.code(),
                    error.message(),
                ),
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
        combine_results(
            combine_results(shutdown_result, join_result.map(|_| ())),
            bye_result,
        ),
    )?;
    if escalated_quit {
        return Ok(ProcessOutcome::EscalatedQuit);
    }
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
    send_navigator: bool,
    topology_available: bool,
) -> Vec<UserAction> {
    let mut actions = Vec::new();
    if send_navigator && topology_available {
        actions.push(UserAction::OpenTopologyNavigator);
    }
    actions
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

struct JsonlSession<'a> {
    busy: &'a mut bool,
    turn_busy: &'a mut bool,
    tree_state: &'a mut String,
    prompt_id: &'a mut Option<String>,
    command_id: Option<&'a String>,
    pending_tree_snapshot_id: &'a mut Option<String>,
    session_id: Option<&'a str>,
}

fn handle_event(event: &HostEvent, session: &mut JsonlSession<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    if let HostEvent::SetBusy(value) = event {
        *session.turn_busy = *value;
        *session.busy = *value;
    }
    if let HostEvent::TopologyNavigatorSnapshot(snapshot) = event {
        *session.tree_state = snapshot.state.as_str().to_owned();
    }
    let correlation = JsonlCorrelation {
        prompt_id: session.prompt_id.as_deref(),
        omit_unknown_turn_id: true,
        command_id: session.command_id.map(String::as_str),
        pending_tree_snapshot_id: session.pending_tree_snapshot_id.as_deref(),
    };
    if let Some(line) = project_host_event(event, correlation, session.session_id) {
        lines.push(line);
        if let HostEvent::TopologyNavigatorSnapshot(snapshot) = event
            && snapshot.solicited
        {
            *session.pending_tree_snapshot_id = None;
        }
    }
    if matches!(event, HostEvent::SetBusy(false)) {
        *session.prompt_id = None;
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
            self.pending.push_back((text.text.trim().to_owned(), id));
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
                    pending == text.text.trim() && self.current_intent.as_ref() == Some(id)
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

fn join_worker(
    worker: plakous_hosted::runtime::HostWorkerHandle,
    deadline: Instant,
) -> CliResult<plakous_hosted::runtime::HostQuitOutcome> {
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
mod tests;
