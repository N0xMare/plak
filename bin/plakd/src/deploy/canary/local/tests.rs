use std::cell::Cell;
use std::time::{Duration, Instant};

use plakous::model::{OutputLocation, ReasoningDelta, TextDelta};

use super::*;

#[tokio::test]
async fn codex_cancel_at_bound_cancels_then_joins_cancelled() {
    let at = OutputLocation::new(0, 0);
    let reason = AgentEventKind::model(ModelEvent::ReasoningDelta(ReasoningDelta::new(
        at,
        vec![b'r'; 40],
    )));
    let text = AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(at, vec![b't'; 24])));
    let mut deliveries = [classify(&reason), classify(&text)].into_iter();
    let cancelled = Cell::new(false);
    drive(
        async || deliveries.next().unwrap(),
        async || {
            cancelled.set(true);
            Ok(())
        },
        async || {
            assert!(cancelled.get());
            Ok(Terminal::Cancelled)
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn codex_cancel_at_bound_accepts_early_terminal_without_cancelling() {
    for terminal in [Terminal::Completed, Terminal::Cancelled] {
        let ended = AgentEventKind::TurnCompleted {
            usage: Default::default(),
        };
        let mut events = [Observation::Bytes(63), classify(&ended)].into_iter();
        let result = drive(
            async || events.next().unwrap(),
            async || panic!("short reply cancelled"),
            async || Ok(terminal),
        )
        .await;
        assert_eq!(result.is_ok(), matches!(terminal, Terminal::Completed));
    }
}

#[tokio::test]
async fn codex_cancel_at_bound_lost_closed_or_content_free_observation_fails_and_shuts_down() {
    let stripped = AgentEventKind::Model {
        kind: NormalizedModelEventKind::ReasoningDelta,
        event: None,
    };
    for (observation, reason) in [
        (Observation::Lagged, "lost output observations"),
        (Observation::Closed, "observation did not settle"),
        (classify(&stripped), "did not receive output bytes"),
    ] {
        let shutdown = Cell::new(false);
        let turn = drive(
            async || observation,
            async || panic!("missing output cancelled as a pass"),
            async || panic!("missing observations ignored"),
        );
        let failed =
            super::super::join_after(Instant::now() + Duration::from_secs(1), turn, || async {
                shutdown.set(true);
                Ok::<_, ()>(())
            })
            .await
            .unwrap_err();
        assert!(failed.message().contains(reason));
        assert!(shutdown.get());
    }
}
