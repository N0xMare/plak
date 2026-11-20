//! Codex has no provider output cap. Limit observed bytes and join cancellation.

use plakous::agent::{AgentEventKind, EventDelivery, NormalizedModelEventKind, Turn};
use plakous::model::ModelEvent;

use crate::{CliFailure, CliResult};

const OBSERVED_BYTES: usize = 64;

pub(super) async fn cancel_at_bound(turn: &mut Turn) -> CliResult<bool> {
    let mut events = turn
        .take_events()
        .map_err(|_| CliFailure::new("cannot observe the Codex canary output"))?;
    let mut observed = OutputBudget(0);
    loop {
        let delivery = events
            .recv_delivery()
            .await
            .map_err(|_| CliFailure::new("Codex canary output observation did not settle"))?;
        let EventDelivery::Event(event) = delivery else {
            return Err(CliFailure::new("Codex canary lost output observations"));
        };
        match event.kind() {
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => {
                return Ok(false);
            }
            AgentEventKind::Model {
                event: Some(model), ..
            } if observed.reached(model) => {
                turn.control()
                    .cancel()
                    .await
                    .map_err(|_| CliFailure::new("Codex canary cancellation did not settle"))?;
                return Ok(true);
            }
            AgentEventKind::Model {
                event: None,
                kind: NormalizedModelEventKind::TextDelta | NormalizedModelEventKind::ReasoningDelta,
            } => return Err(CliFailure::new("Codex canary did not receive output bytes")),
            _ => {}
        }
    }
}

struct OutputBudget(usize);

impl OutputBudget {
    fn reached(&mut self, event: &ModelEvent) -> bool {
        let bytes = match event {
            ModelEvent::TextDelta(delta) => delta.bytes().len(),
            ModelEvent::ReasoningDelta(delta) => delta.bytes().len(),
            _ => 0,
        };
        self.0 = self.0.saturating_add(bytes);
        self.0 >= OBSERVED_BYTES
    }
}

#[cfg(test)]
#[test]
fn codex_canary_combines_reasoning_and_text_at_the_local_threshold() {
    use plakous::model::{OutputLocation, ReasoningDelta, TextDelta};

    let mut bound = OutputBudget(0);
    let at = OutputLocation::new(0, 0);
    assert!(
        !bound.reached(&ModelEvent::ReasoningDelta(ReasoningDelta::new(
            at,
            vec![b'r'; 40]
        )))
    );
    assert!(!bound.reached(&ModelEvent::TextDelta(TextDelta::new(at, vec![b't'; 23]))));
    assert!(bound.reached(&ModelEvent::TextDelta(TextDelta::new(at, b"!".to_vec()))));
}
