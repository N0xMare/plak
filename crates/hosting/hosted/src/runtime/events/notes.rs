//! Operator notes shared by event and recovery paths.

use crate::session_log::SessionEvent;

/// What the operator sees when a turn spends its model-operation budget.
///
/// The runtime injects a note for the model at the same moment, but canonical
/// history is not what the transcript or the session log reads. Without this
/// line the user gets a wrap-up answer and no reason for it, and a resumed
/// session shows nothing at all.
pub(super) const OPERATION_BUDGET_EXHAUSTED_NOTE: &str =
    "turn reached its model-operation budget; answering with findings so far";

pub(super) fn output_limit_retry_note(bound: Option<std::num::NonZeroU32>) -> String {
    bound.map_or_else(
        || "response reached the provider output limit before completion; retrying with less output".to_owned(),
        |bound| {
            format!(
                "response reached its {}-token output limit before completion; retrying with less output",
                bound.get()
            )
        },
    )
}

/// The one line an operator sees when a reopened session's route moved its budget.
///
/// The session keeps the figure it was created with. That figure reaches the
/// configuration identity and from there the persisted harness plan, the tool
/// projection's provider target, the request prefix and the admission evidence,
/// none of which can be re-signed on reopen, so adopting the route's new figure
/// would make the session unopenable rather than roomier. An old session on a
/// route that has since grown is therefore starved, and the operator needs to
/// be told both figures and the one action that gets the larger one.
pub(crate) fn output_bound_rederived_note(
    previous: Option<std::num::NonZeroU32>,
    current: Option<std::num::NonZeroU32>,
) -> String {
    let render = |bound: Option<std::num::NonZeroU32>| {
        bound.map_or_else(|| "none".to_owned(), |bound| bound.get().to_string())
    };
    format!(
        "plak: this session keeps its output budget of {} tokens; the route now attests {}; \
         start a new session to use it",
        render(previous),
        render(current)
    )
}

/// The operator line for a tool result the observation budget truncated.
///
/// Says which tool, how much went and how much stayed, and against what
/// budget, because "truncated" without a figure tells a reader nothing about
/// whether the model saw enough. The model is answering from what remains, so
/// the line also says what to do about it.
pub(super) fn observation_clipped_note(
    name: &str,
    bytes_kept: u64,
    bytes_dropped: u64,
    bound: u64,
) -> String {
    format!(
        "observation clipped: {name} returned more than the remaining observation budget; \
         {bytes_kept} bytes kept, {bytes_dropped} dropped, against a {bound}-byte bound. \
         The model is answering from the kept portion; narrow the call to see the rest."
    )
}

pub(super) fn observation_replaced_record(
    call_id: Option<&str>,
    bytes_before: u64,
    bytes_after: u64,
) -> SessionEvent {
    SessionEvent::new("observation.replaced")
        .opt_str("call_id", call_id)
        .u64("bytes_before", bytes_before)
        .u64("bytes_after", bytes_after)
}

/// The one line the operator reads when an unfinished turn is closed. Exactly
/// one of these is true of a cut, and each says only its own thing: a
/// workspace warning over a command that provably never started would teach
/// the operator to skip the warning that matters.
pub(crate) const fn abandoned_turn_operator_line(
    report: &plakous::agent::AbandonedTurnReport,
) -> &'static str {
    abandoned_turn_operator_detail(
        report.claim(),
        report.cause(),
        report.tool_round_completed(),
        report.model_round_completed(),
    )
}

pub(crate) const fn abandoned_turn_operator_detail(
    claim: plakous::agent::AbandonedTurnClaim,
    cause: plakous::agent::AbandonmentCause,
    tool_round_completed: bool,
    model_round_completed: bool,
) -> &'static str {
    if tool_round_completed && matches!(claim, plakous::agent::AbandonedTurnClaim::RunningTool) {
        return "plak: recovered an interrupted turn; its tool round completed before the next \
                model request, but a process it started has an unknown outcome; completed tool \
                results were kept; check the workspace before carrying on";
    }
    match (claim, cause) {
        (plakous::agent::AbandonedTurnClaim::ToolRoundCompleted, _) if !model_round_completed => {
            "plak: recovered an interrupted turn; its tool round completed, but the turn was \
             interrupted before the next model request; completed tool results were kept"
        }
        (plakous::agent::AbandonedTurnClaim::ToolRoundCompleted, _) => {
            "plak: recovered an interrupted turn; its last round completed, but the turn was \
             interrupted before the next model request; completed work was kept"
        }
        (plakous::agent::AbandonedTurnClaim::RunningTool, _) => {
            "plak: recovered an interrupted turn; a tool may have changed this workspace; \
             its result was not added to the conversation and it is not retried - check \
             the workspace before carrying on"
        }
        (plakous::agent::AbandonedTurnClaim::ModelRequest, _) => {
            "plak: recovered an interrupted turn; its model request may have been processed \
             and billed; nothing from it was kept"
        }
        (plakous::agent::AbandonedTurnClaim::CompactionNotApplied, _) => {
            "plak: recovered an interrupted turn; compaction was not applied (it may have been \
             billed) and the conversation is unchanged"
        }
        (_, plakous::agent::AbandonmentCause::ProcessLost) => {
            "plak: recovered an interrupted turn; exact evidence proves its pending work \
             never started, so nothing after that boundary ran"
        }
        _ => {
            "plak: recovered an interrupted turn; nothing it asked for had started, so nothing \
             ran and nothing was changed"
        }
    }
}

#[cfg(test)]
mod tests;
