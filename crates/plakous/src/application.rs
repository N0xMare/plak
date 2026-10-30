//! Neutral application-edge execution and caller-supplied cancellation.
//!
//! This module exists so a facade-only application can drive Plakous futures
//! without depending directly on Plakous's async executor. It owns no agent,
//! model, provider, session, retry, or tool-loop behavior.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::time::Duration;

use plakous_agent::{
    Agent, AgentError, AgentEvent, ControlError, RUNTIME_EXECUTOR_STACK_BYTES, TurnResult,
    TurnResultOutcome, UserInput,
};
use tokio::runtime::{Builder, Handle, Runtime};

/// Worker count for the application-edge runtime.
///
/// Tokio's default is one worker per logical CPU. A CLI/TUI on a many-core
/// host would otherwise spawn a hundred-plus threads during attach, which
/// dominates ready time and RSS. Four workers cover the single-agent loop,
/// HTTP, and a few overlapping `spawn_blocking` tools.
const APPLICATION_WORKER_THREADS: usize = 4;

/// Quit cannot sit on Tokio's default `Runtime` drop, which waits forever for
/// `spawn_blocking` (SQLite, HTTP). Leftover blocking work is leaked and the
/// process reaps it; unfinished writes reconcile from the checkpoint.
const APPLICATION_EXECUTOR_SHUTDOWN: Duration = Duration::from_millis(250);

/// An application-edge executor for one top-level synchronous process.
///
/// Library consumers that already run inside an async executor should call the
/// ordinary async Plakous APIs and do not need this type.
pub struct ApplicationExecutor {
    runtime: Option<Runtime>,
}

impl ApplicationExecutor {
    /// Constructs one multi-thread Tokio executor with time and I/O enabled,
    /// whose threads carry [`RUNTIME_EXECUTOR_STACK_BYTES`] of stack.
    ///
    /// # Errors
    ///
    /// Returns [`ApplicationExecutorError::AlreadyRunning`] when called from a
    /// Tokio task, or [`ApplicationExecutorError::Construction`] when the
    /// executor cannot allocate its runtime resources.
    pub fn new() -> Result<Self, ApplicationExecutorError> {
        if Handle::try_current().is_ok() {
            return Err(ApplicationExecutorError::AlreadyRunning);
        }
        let runtime = Builder::new_multi_thread()
            .worker_threads(APPLICATION_WORKER_THREADS)
            // Worker and blocking threads both poll the agent's turn loop; its
            // unoptimized poll frames outgrow the 2 MiB `std::thread` default
            // (see `RUNTIME_EXECUTOR_STACK_BYTES`). Virtual reservation only.
            .thread_stack_size(RUNTIME_EXECUTOR_STACK_BYTES)
            .enable_all()
            .build()
            .map_err(ApplicationExecutorError::Construction)?;
        Ok(Self {
            runtime: Some(runtime),
        })
    }

    /// Drives one application future to completion.
    ///
    /// This method is only valid at the synchronous application edge.
    ///
    /// # Errors
    ///
    /// Returns [`ApplicationExecutorError::AlreadyRunning`] instead of nesting
    /// a blocking executor inside an active Tokio task.
    pub fn block_on<F>(&self, future: F) -> Result<F::Output, ApplicationExecutorError>
    where
        F: Future,
    {
        if Handle::try_current().is_ok() {
            return Err(ApplicationExecutorError::AlreadyRunning);
        }
        match &self.runtime {
            Some(runtime) => Ok(runtime.block_on(future)),
            None => Err(ApplicationExecutorError::AlreadyRunning),
        }
    }
}

impl Drop for ApplicationExecutor {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(APPLICATION_EXECUTOR_SHUTDOWN);
        }
    }
}

impl fmt::Debug for ApplicationExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationExecutor")
            .finish_non_exhaustive()
    }
}

/// Failure to construct or safely enter an application executor.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApplicationExecutorError {
    /// A blocking application executor was requested from an active Tokio task.
    AlreadyRunning,
    /// Tokio could not construct the executor.
    Construction(io::Error),
}

impl fmt::Display for ApplicationExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => {
                formatter.write_str("application executor cannot be nested in a Tokio task")
            }
            Self::Construction(_) => {
                formatter.write_str("application executor construction failed")
            }
        }
    }
}

impl Error for ApplicationExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AlreadyRunning => None,
            Self::Construction(error) => Some(error),
        }
    }
}

/// Terminal result of one application-owned agent turn.
pub enum TurnDriveOutcome {
    /// The model/tool loop committed a complete result.
    Completed(Box<TurnResult>),
    /// The supplied signal prevented admission or cancelled and joined the exact turn.
    Interrupted,
}

impl fmt::Debug for TurnDriveOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed(result) => formatter.debug_tuple("Completed").field(&result).finish(),
            Self::Interrupted => formatter.write_str("Interrupted"),
        }
    }
}

/// Failure while presenting and joining one application-owned turn.
#[derive(Debug)]
#[non_exhaustive]
pub enum TurnDriveError {
    /// Prompt admission or terminal agent execution failed.
    Agent(AgentError),
    /// Exact-turn cancellation could not be acknowledged.
    Control(ControlError),
    /// The presentation callback failed.
    Presentation(io::Error),
    /// Ctrl-C registration or delivery failed.
    Signal(io::Error),
    /// Agent shutdown could not join cleanly.
    Shutdown(AgentError),
}

impl fmt::Display for TurnDriveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Agent(_) => "agent turn failed",
            Self::Control(_) => "turn cancellation failed",
            Self::Presentation(_) => "turn presentation failed",
            Self::Signal(_) => "interrupt handling failed",
            Self::Shutdown(_) => "agent shutdown failed",
        })
    }
}

impl Error for TurnDriveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Agent(error) | Self::Shutdown(error) => Some(error),
            Self::Control(error) => Some(error),
            Self::Presentation(error) | Self::Signal(error) => Some(error),
        }
    }
}

/// Runs, presents, interrupts, and joins one prompt on an owned agent.
///
/// The callback receives available content-projected causal events in order.
/// Observation attachment, lag, closure, or failure never delays or replaces
/// the authoritative turn result. The cancellation signal is polled before prompt admission. If
/// admission wins, the signal requests exact-turn cancellation and the runner
/// continues to await the result. Agent shutdown is always joined before
/// returning.
///
/// # Errors
///
/// Returns a typed presentation, signal, control, agent, or shutdown failure
/// after requesting bounded cleanup.
pub async fn run_once_until_signal<F, S>(
    agent: Agent,
    prompt: impl Into<UserInput>,
    signal: S,
    mut present: F,
) -> Result<TurnDriveOutcome, TurnDriveError>
where
    F: FnMut(&AgentEvent) -> io::Result<()>,
    S: Future<Output = io::Result<()>>,
{
    tokio::pin!(signal);
    let prompt = agent.prompt(prompt);
    tokio::pin!(prompt);
    let prompt_result = tokio::select! {
        biased;
        signal_result = &mut signal => {
            if let Err(error) = agent.shutdown().await {
                return Err(TurnDriveError::Shutdown(error));
            }
            return match signal_result {
                Ok(()) => Ok(TurnDriveOutcome::Interrupted),
                Err(error) => Err(TurnDriveError::Signal(error)),
            };
        }
        prompt_result = &mut prompt => prompt_result,
    };
    let mut turn = match prompt_result {
        Ok(turn) => turn,
        Err(error) => {
            return match agent.shutdown().await {
                Ok(()) => Err(TurnDriveError::Agent(error)),
                Err(shutdown) => Err(TurnDriveError::Shutdown(shutdown)),
            };
        }
    };
    let mut events = turn.take_events().ok();
    let control = turn.control();
    let result = turn.result();
    tokio::pin!(result);
    let mut signal_active = true;
    let mut cancellation_requested = false;
    let mut cancellation_attempted = false;
    let loop_result = loop {
        tokio::select! {
            biased;
            result_value = &mut result => break Ok(result_value),
            signal_result = &mut signal, if signal_active => {
                signal_active = false;
                if let Err(error) = signal_result {
                    break Err(TurnDriveError::Signal(error));
                }
                cancellation_attempted = true;
                match control.cancel().await {
                    Ok(_) => {
                        cancellation_requested = true;
                    }
                    Err(error) => break Err(TurnDriveError::Control(error)),
                }
            }
            event_result = async {
                match events.as_mut() {
                    Some(events) => Some(events.recv().await),
                    None => None,
                }
            }, if events.is_some() => {
                let Some(event_result) = event_result else {
                    continue;
                };
                match event_result {
                    Ok(event) => {
                        let observation_ended = matches!(
                            event.kind(),
                            plakous_agent::AgentEventKind::TurnCompleted { .. }
                                | plakous_agent::AgentEventKind::TurnFailed { .. }
                        );
                        if let Err(error) = present(&event) {
                            break Err(TurnDriveError::Presentation(error));
                        }
                        if observation_ended {
                            events = None;
                        }
                    }
                    Err(_) => events = None,
                }
            }
        }
    };
    if loop_result.is_err() && !cancellation_attempted {
        let _ = control.cancel().await;
    }
    agent.shutdown().await.map_err(TurnDriveError::Shutdown)?;
    let result = loop_result?.map_err(TurnDriveError::Agent)?;
    match result.outcome() {
        TurnResultOutcome::Completed(_) => Ok(TurnDriveOutcome::Completed(Box::new(result))),
        TurnResultOutcome::Cancelled(_) if cancellation_requested => {
            Ok(TurnDriveOutcome::Interrupted)
        }
        TurnResultOutcome::Cancelled(error)
        | TurnResultOutcome::Failed(error)
        | TurnResultOutcome::RecoveryRequired(error) => Err(TurnDriveError::Agent(error.clone())),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
