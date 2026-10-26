//! Opt-in cost of continuously reducing children, including headless hosts.

use super::*;
use plakous::agent::AgentEventKind;
use plakous::model::{ModelEvent, OutputLocation, TextDelta};
use std::time::Instant;

#[tokio::test]
#[ignore = "opt-in observational performance evidence, not a latency gate"]
async fn streaming_children_and_faces_cost() {
    const CHILDREN: usize = 8;
    const ROUNDS: usize = 1024;
    let host = std::env::var("PLAKOUS_PERF_HOST_ID").expect("explicit benchmark host identity");
    for faces in [0, 1, 3] {
        let mut owners = Vec::new();
        let mut sources = Vec::new();
        let mut tails = Vec::new();
        for child in 0..CHILDREN {
            let turn = TurnId::try_from(uuid::Uuid::from_u128(100 + child as u128)).unwrap();
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            let owner = ChildTurnProjection::start(
                turn,
                None,
                std::future::ready(Some(Source::Fixture(receiver))),
            );
            tails.push(
                (0..faces)
                    .map(|_| owner.observation.attach().1)
                    .collect::<Vec<_>>(),
            );
            sources.push((
                sender,
                tests::event(
                    turn,
                    AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(
                        OutputLocation::new(0, 0),
                        vec![b'x'; 64],
                    ))),
                ),
            ));
            owners.push(owner);
        }
        let started = Instant::now();
        for round in 1..=ROUNDS {
            for (sender, event) in &sources {
                sender.send(Received::Event(event.clone())).await.unwrap();
            }
            for (owner, tails) in owners.iter().zip(&mut tails) {
                while owner.observation.snapshot().cursor != round as u64 {
                    tokio::task::yield_now().await;
                }
                for tail in tails {
                    assert!(tail.recv().await.unwrap().is_some());
                }
            }
        }
        let elapsed = started.elapsed().as_nanos();
        // Account the retained text workload, not allocator metadata or RSS.
        // Tails share each Arc-backed update; they never clone payload strings.
        let retained_bytes: usize = owners
            .iter()
            .map(|owner| {
                let state = owner.observation.lock();
                size_of::<State>()
                    + size_of::<TurnProjection<Core>>()
                    + state.updates.capacity() * size_of::<Update>()
                    + state
                        .updates
                        .iter()
                        .map(|update| {
                            2 * size_of::<usize>()
                                + size_of_val(update.ops.as_ref())
                                + update
                                    .ops
                                    .iter()
                                    .map(|op| match op {
                                        HostEvent::AssistantDelta(text) => text.capacity(),
                                        _ => 0,
                                    })
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
            + CHILDREN * faces * size_of::<Tail>();
        println!(
            "{}",
            serde_json::json!({
                "schema":"plakous.perf-child-observation.v1", "host_id":host,
                "children":CHILDREN, "faces":faces, "events":CHILDREN * ROUNDS,
                "text_bytes_per_event":64, "elapsed_ns":elapsed,
                "per_event_ns":elapsed / (CHILDREN * ROUNDS) as u128,
                "retained_bytes":retained_bytes,
                "retained_bytes_scope":"state, inline reducer, ring capacity, shared update allocations, text payload capacities and tail values; excludes allocator overhead, task stacks and RSS",
                "timing_scope":"source delivery, child reduction, snapshot observation and face tail reads; no providers, stores or terminal rendering",
                "headless_reduces":true,
            })
        );
    }
}
