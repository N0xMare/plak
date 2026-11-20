//! Codex has no provider output cap. Limit observed bytes and join cancellation.

use plakous::agent::{
    AgentEventKind, EventDelivery, NormalizedModelEventKind, Turn, TurnResultOutcome,
};
use plakous::model::ModelEvent;

use crate::{CliFailure, CliResult};

const OBSERVED_BYTES: usize = 64;

pub(super) async fn cancel_at_bound(mut turn: Turn) -> CliResult<()> {
    let mut events = turn
        .take_events()
        .map_err(|_| CliFailure::new("cannot observe the Codex canary output"))?;
    let control = turn.control();
    drive(
        async || match events.recv_delivery().await {
            Ok(EventDelivery::Event(event)) => classify(event.kind()),
            Ok(_) => Observation::Lagged,
            Err(_) => Observation::Closed,
        },
        || async {
            control
                .cancel()
                .await
                .map_err(|_| CliFailure::new("Codex canary cancellation did not settle"))?;
            Ok(())
        },
        || async {
            let result = turn
                .result()
                .await
                .map_err(|_| CliFailure::new("canary turn did not settle"))?;
            Ok(match result.outcome() {
                TurnResultOutcome::Completed(_) => Terminal::Completed,
                TurnResultOutcome::Cancelled(_) => Terminal::Cancelled,
                _ => Terminal::Failed,
            })
        },
    )
    .await
}

#[derive(Clone, Copy)]
enum Observation {
    Bytes(usize),
    Terminal,
    Lagged,
    Closed,
    MissingBytes,
}

#[derive(Clone, Copy)]
enum Terminal {
    Completed,
    Cancelled,
    Failed,
}

async fn drive(
    mut next: impl AsyncFnMut() -> Observation,
    cancel: impl AsyncFnOnce() -> CliResult<()>,
    join: impl AsyncFnOnce() -> CliResult<Terminal>,
) -> CliResult<()> {
    let mut observed = 0_usize;
    let hit_bound = loop {
        match next().await {
            Observation::Bytes(bytes) => {
                observed = observed.saturating_add(bytes);
                if observed >= OBSERVED_BYTES {
                    cancel().await?;
                    break true;
                }
            }
            Observation::Terminal => break false,
            Observation::Lagged => {
                return Err(CliFailure::new("Codex canary lost output observations"));
            }
            Observation::Closed => {
                return Err(CliFailure::new(
                    "Codex canary output observation did not settle",
                ));
            }
            Observation::MissingBytes => {
                return Err(CliFailure::new("Codex canary did not receive output bytes"));
            }
        }
    };
    match join().await? {
        Terminal::Completed => Ok(()),
        Terminal::Cancelled if hit_bound => Ok(()),
        _ => Err(CliFailure::new("canary turn did not complete")),
    }
}

fn classify(event: &AgentEventKind) -> Observation {
    match event {
        AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => {
            Observation::Terminal
        }
        AgentEventKind::Model {
            event: Some(model), ..
        } => Observation::Bytes(match model.as_ref() {
            ModelEvent::TextDelta(delta) => delta.bytes().len(),
            ModelEvent::ReasoningDelta(delta) => delta.bytes().len(),
            _ => 0,
        }),
        AgentEventKind::Model {
            event: None,
            kind: NormalizedModelEventKind::TextDelta | NormalizedModelEventKind::ReasoningDelta,
        } => Observation::MissingBytes,
        _ => Observation::Bytes(0),
    }
}

#[cfg(test)]
mod tests;
