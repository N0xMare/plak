//! A single root turn driven by the attached host, including publication and release.

use std::time::Duration;

use crate::error::{CliFailure, CliFailureKind, CliResult};
use crate::surface_protocol::{
    UserAction, UserActionSender, host_event_channel, user_action_channel,
};

use super::{HostConfig, HostEvent, Sender, TurnHostOutcome};

/// Bounds for the non-interactive run-until-idle face.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunBounds {
    pub max_turns: u8,
    /// Relinquish leftover process-group ownership at run exit.
    pub leave_jobs_running: bool,
    /// Absolute whole-run deadline, established before product preparation.
    pub deadline: Option<std::time::Instant>,
}

impl Default for RunBounds {
    fn default() -> Self {
        Self {
            max_turns: 25,
            leave_jobs_running: false,
            deadline: None,
        }
    }
}

/// Authoritative result after the product record was published and the session released.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OneShotOutcome {
    /// The root turn completed with its canonical visible text.
    Completed { assistant: String },
    /// A committed visible answer stopped at its output limit.
    Truncated { assistant: String },
    /// The provider refused; its canonical terminal was published and released.
    Refused {
        assistant: String,
        detail: Option<String>,
    },
    /// Preparation stopped before admission, or cancellation settled and its
    /// terminal projection was committed.
    Interrupted { assistant: String },
    /// The whole run reached an operator bound after canonical settlement.
    Bounded { assistant: String, bound: String },
}

pub(super) struct OneShotState {
    pub attempted: bool,
    pub resume_tree: bool,
    pub result: Option<CliResult<OneShotOutcome>>,
    pub run: Option<RunState>,
}

pub(super) struct RunState {
    pub bounds: RunBounds,
    pub deadline: Option<std::time::Instant>,
    pub turns: u8,
    pub timed_out: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// The one-shot face of the attached host. Observations never authorize completion.
pub struct OneShot;

impl OneShot {
    /// Drive one prompt through the same admission, turn and publication path as
    /// an interactive session. The callback may send controls such as Interrupt
    /// or ResolveApproval; it is also called on idle polls with no observation.
    pub fn run<P: plakous_host_protocol::ProtocolExtension>(
        config: HostConfig<P>,
        prompt: String,
        observe: impl FnMut(Option<&HostEvent<P>>, &UserActionSender<P>),
    ) -> CliResult<OneShotOutcome> {
        Self::drive(config, prompt.into(), false, None, observe)
    }

    /// Explicitly resume the attached tree after canonical opening, then drive one prompt.
    pub fn run_resuming_tree<P: plakous_host_protocol::ProtocolExtension>(
        config: HostConfig<P>,
        prompt: String,
        observe: impl FnMut(Option<&HostEvent<P>>, &UserActionSender<P>),
    ) -> CliResult<OneShotOutcome> {
        Self::drive(config, prompt.into(), true, None, observe)
    }

    /// Admit distinct continuation turns until no processes or children remain.
    pub fn run_until_idle<P: plakous_host_protocol::ProtocolExtension>(
        config: HostConfig<P>,
        prompt: plakous_host_protocol::Draft,
        resume_tree: bool,
        bounds: RunBounds,
        observe: impl FnMut(Option<&HostEvent<P>>, &UserActionSender<P>),
    ) -> CliResult<OneShotOutcome> {
        if !(1..=100).contains(&bounds.max_turns) {
            return Err(CliFailure::classified(
                CliFailureKind::Usage,
                "max-turns must be 1..=100",
            ));
        }
        Self::drive(config, prompt, resume_tree, Some(bounds), observe)
    }

    fn drive<P: plakous_host_protocol::ProtocolExtension>(
        mut config: HostConfig<P>,
        prompt: plakous_host_protocol::Draft,
        resume_tree: bool,
        bounds: Option<RunBounds>,
        mut observe: impl FnMut(Option<&HostEvent<P>>, &UserActionSender<P>),
    ) -> CliResult<OneShotOutcome> {
        if prompt.text.trim().is_empty() && prompt.images.is_empty() {
            return Err(CliFailure::classified(
                CliFailureKind::Usage,
                "prompt is empty",
            ));
        }
        if prompt.text.len() > super::MAX_PROMPT_BYTES {
            return Err(CliFailure::classified(
                CliFailureKind::Usage,
                "prompt exceeds the hosted input bound",
            ));
        }
        config.attached_one_shot = true;
        let deadline = bounds.and_then(|bounds| bounds.deadline);
        let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_timed_out = timed_out.clone();
        let (controls, actions) = user_action_channel();
        let (events, observations) = host_event_channel();
        controls
            .send(UserAction::Prompt(prompt))
            .map_err(|_| CliFailure::new("host control channel closed"))?;
        let worker = std::thread::spawn(move || {
            let mut state = OneShotState {
                attempted: false,
                resume_tree,
                result: None,
                run: bounds.map(|bounds| RunState {
                    bounds,
                    deadline,
                    turns: 0,
                    timed_out: worker_timed_out,
                }),
            };
            let released = super::host_loop::run(
                config,
                &actions,
                Sender::over(events),
                None,
                Some(&mut state),
            );
            let Some(terminal) = state.result else {
                return released
                    .map_err(|error| error.or_kind(CliFailureKind::Agent))
                    .and_then(|()| {
                        Err(CliFailure::classified(
                            CliFailureKind::Agent,
                            "one-shot prompt was not admitted",
                        ))
                    });
            };
            match (terminal, released) {
                (Ok(result), Ok(())) => Ok(result),
                (Err(error), Ok(())) | (Ok(_), Err(error)) => {
                    Err(error.or_kind(CliFailureKind::Agent))
                }
                (Err(terminal), Err(release)) => Err(terminal
                    .with_release_failure(release)
                    .or_kind(CliFailureKind::Agent)),
            }
        });
        while !worker.is_finished() {
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline)
                && !timed_out.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                let _ = controls.send(UserAction::Interrupt);
            }
            match observations.recv_timeout(Duration::from_millis(20)) {
                Ok(event) => observe(Some(&event), &controls),
                Err(_) => observe(None, &controls),
            }
        }
        let result = worker
            .join()
            .map_err(|_| CliFailure::new("host worker panicked"))?;
        for event in observations.try_iter() {
            observe(Some(&event), &controls);
        }
        result
    }
}

pub(super) fn terminal(drive: &Result<TurnHostOutcome, String>) -> CliResult<OneShotOutcome> {
    match drive {
        Ok(TurnHostOutcome::ConsumedDelivery) => Err(CliFailure::classified(
            CliFailureKind::Agent,
            "queued prompt already ran; its result is no longer kept",
        )),
        Ok(TurnHostOutcome::Completed {
            assistant,
            finish_reason: plakous::model::FinishReason::Refusal,
            finish_detail,
            ..
        }) => Ok(OneShotOutcome::Refused {
            assistant: assistant.clone(),
            detail: finish_detail.clone(),
        }),
        Ok(TurnHostOutcome::Completed {
            assistant,
            finish_reason: plakous::model::FinishReason::Length,
            ..
        }) => Ok(OneShotOutcome::Truncated {
            assistant: assistant.clone(),
        }),
        Ok(TurnHostOutcome::Completed { assistant, .. }) => Ok(OneShotOutcome::Completed {
            assistant: assistant.clone(),
        }),
        Ok(TurnHostOutcome::Interrupted { assistant, .. }) => Ok(OneShotOutcome::Interrupted {
            assistant: assistant.clone(),
        }),
        Ok(TurnHostOutcome::RecoveryRequired { error }) => Err(CliFailure::recovery_required(
            format!("{error}; the session recovers automatically the next time it is used"),
        )),
        Ok(TurnHostOutcome::AdmissionRejected { error, .. })
        | Ok(TurnHostOutcome::Failed { error, .. }) => {
            Err(CliFailure::classified(CliFailureKind::Agent, error.clone()))
        }
        Ok(TurnHostOutcome::ContextCapacity { error, .. }) | Err(error) => {
            Err(CliFailure::classified(CliFailureKind::Agent, error.clone()))
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn non_admitted_and_consumed_deliveries_are_classified_failures() {
        for outcome in [
            TurnHostOutcome::AdmissionRejected {
                stop_drain: false,
                error: "prompt was not admitted: storage refusal".into(),
            },
            TurnHostOutcome::ConsumedDelivery,
        ] {
            let error = terminal(&Ok(outcome)).unwrap_err();
            assert_ne!(error.exit_code(), 0);
            assert!(
                error.message().contains("not admitted")
                    || error.message() == "queued prompt already ran; its result is no longer kept"
            );
        }
    }
}
