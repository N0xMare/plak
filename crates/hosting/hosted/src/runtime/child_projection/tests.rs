#![allow(
    clippy::too_many_lines,
    reason = "Child projection scenarios force event-prefix eviction and attach races while preserving exact committed counters."
)]

use super::*;
use crate::runtime::child_live::tests::tool_lifecycle_event;
use plakous::agent::{AgentEventKind, ToolCallerLineageView};
use plakous::model::{ModelEvent, OutputLocation, TextDelta, Usage, UsageUpdate};
use std::collections::HashSet;
use std::time::Duration;

#[cfg(test)]
mod attach_fences;

pub(super) fn event(turn: TurnId, kind: AgentEventKind) -> Arc<AgentEvent> {
    let base = tool_lifecycle_event(
        "read_file",
        &uuid::Uuid::from_u128(2).to_string(),
        "ToolStarted",
        &ToolCallerLineageView::direct(),
        serde_json::json!({"arguments_preview":"{}","subject":"first"}),
    );
    let mut value = serde_json::to_value(base).unwrap();
    value["turn_id"] = serde_json::to_value(turn).unwrap();
    value["attempt_id"] = serde_json::json!(uuid::Uuid::from_u128(3));
    value["kind"] = serde_json::to_value(kind).unwrap();
    Arc::new(serde_json::from_value(value).unwrap())
}

async fn publish(
    sender: &tokio::sync::mpsc::Sender<Received>,
    observation: &ChildObservation,
    event: Arc<AgentEvent>,
) {
    let expected = observation.snapshot().cursor + 1;
    sender.send(Received::Event(event)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while observation.snapshot().cursor != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn credential_wait_labels_snapshot_without_rewriting_retained_updates() {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0xc8)).unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let owner = ChildTurnProjection::start(
        turn,
        None,
        std::future::ready(Some(Source::Fixture(receiver))),
    );
    let observation = &owner.observation;
    publish(
        &sender,
        observation,
        event(turn, AgentEventKind::ModelCredentialWait { waiting: true }),
    )
    .await;
    let before = observation.snapshot();
    assert_eq!(
        before.progress.activity.as_deref(),
        Some("waiting for login")
    );
    let retained = observation.lock().updates.back().unwrap().ops.clone();
    observation.set_login_account("claude");
    let (snapshot, _) = observation.attach();
    assert_eq!(
        snapshot.cursor, before.cursor,
        "a label does not manufacture another event"
    );
    assert_eq!(
        snapshot.progress.activity.as_deref(),
        Some("waiting for login")
    );
    assert_eq!(snapshot.progress.credential_wait.as_deref(), Some("claude"));
    assert!(!snapshot.terminal);
    {
        let state = observation.lock();
        let update = state.updates.back().unwrap();
        assert!(Arc::ptr_eq(&retained, &update.ops));
        assert!(
            matches!(&update.ops[0], HostEvent::Row(crate::sessions::TranscriptRow::Notice(notice))
            if notice.text == "waiting for login")
        );
    }
    publish(
        &sender,
        observation,
        event(turn, AgentEventKind::ModelCredentialWait { waiting: false }),
    )
    .await;
    assert!(observation.snapshot().progress.activity.is_none());
    assert!(!observation.snapshot().terminal);
}

#[tokio::test]
async fn a_late_in_flight_face_keeps_exact_counters_after_the_event_prefix_is_evicted() {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(7)).unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let owner = ChildTurnProjection::start(
        turn,
        None,
        std::future::ready(Some(Source::Fixture(receiver))),
    );
    let observation = &owner.observation;
    let (_, mut stale) = observation.attach();
    publish(
        &sender,
        observation,
        event(turn, AgentEventKind::ModelDispatchStarted),
    )
    .await;
    for output in [2, 3] {
        publish(
            &sender,
            observation,
            event(
                turn,
                AgentEventKind::model(ModelEvent::Usage(UsageUpdate::new(Usage::new(
                    11, output, 1, 7, 0,
                )))),
            ),
        )
        .await;
    }
    let started = tool_lifecycle_event(
        "read_file",
        &uuid::Uuid::from_u128(2).to_string(),
        "ToolStarted",
        &ToolCallerLineageView::direct(),
        serde_json::json!({"arguments_preview":"{}","subject":"first"}),
    );
    publish(&sender, observation, event(turn, started.kind().clone())).await;
    publish(&sender, observation, event(turn, started.kind().clone())).await;
    publish(
        &sender,
        observation,
        event(turn, AgentEventKind::ModelDispatchStarted),
    )
    .await;
    publish(
        &sender,
        observation,
        event(
            turn,
            AgentEventKind::model(ModelEvent::Usage(UsageUpdate::new(Usage::new(
                17, 5, 2, 9, 0,
            )))),
        ),
    )
    .await;
    for _ in 0..=RETAINED_UPDATES {
        publish(
            &sender,
            observation,
            event(
                turn,
                AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(
                    OutputLocation::new(0, 0),
                    b"x".to_vec(),
                ))),
            ),
        )
        .await;
    }
    assert!(
        matches!(stale.recv().await, Err(())),
        "the initial tool and usage observations are no longer retained"
    );
    let (snapshot, mut first) = observation.attach();
    let (second_snapshot, mut second) = observation.attach();
    assert_eq!(snapshot.cursor, second_snapshot.cursor);
    assert!(!snapshot.terminal, "the child remains in flight");
    assert_eq!(snapshot.progress.turn_id, turn.to_string());
    assert_eq!(snapshot.progress.tool_calls, 1);
    assert_eq!(snapshot.progress.input_tokens, 28);
    assert_eq!(snapshot.progress.output_tokens, 8);
    assert_eq!(snapshot.progress.cached_input_tokens, 16);
    publish(
        &sender,
        observation,
        event(
            turn,
            AgentEventKind::model(ModelEvent::Usage(UsageUpdate::new(Usage::new(
                17, 6, 2, 9, 0,
            )))),
        ),
    )
    .await;
    let first = first.recv().await.unwrap().unwrap();
    let second = second.recv().await.unwrap().unwrap();
    assert_eq!(first.cursor, snapshot.cursor + 1);
    assert_eq!(first.cursor, second.cursor);
    assert_eq!(first.ops, second.ops);
    assert!(first.ops.iter().any(|op| matches!(
        op,
        HostEvent::TurnUsage {
            output_tokens: 9,
            terminal: false,
            ..
        }
    )));
}

#[tokio::test]
async fn faces_do_not_keep_a_retired_child_projection_alive() {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(7)).unwrap();
    let (_sender, receiver) = tokio::sync::mpsc::channel(1);
    let owner = ChildTurnProjection::start(
        turn,
        None,
        std::future::ready(Some(Source::Fixture(receiver))),
    );
    let observation = owner.observation.clone();
    let (_, mut tail) = observation.attach();
    drop(owner);
    assert!(
        observation.lock().projection.is_none(),
        "a face retains only the closed read model, not the reducer"
    );
    assert!(observation.snapshot().terminal);
    assert!(tail.recv().await.unwrap().is_none());
}

async fn observed(observation: &ChildObservation, predicate: impl Fn(&State) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate(&observation.lock()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn late_attach_keeps_a_running_call_after_its_event_prefix_is_evicted() {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0xc9)).unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let owner = ChildTurnProjection::start(
        turn,
        None,
        std::future::ready(Some(Source::Fixture(receiver))),
    );
    let observation = &owner.observation;
    let started = tool_lifecycle_event(
        "read_file",
        &uuid::Uuid::from_u128(2).to_string(),
        "ToolStarted",
        &ToolCallerLineageView::direct(),
        serde_json::json!({"arguments_preview":"{}","subject":"held"}),
    );
    publish(&sender, observation, event(turn, started.kind().clone())).await;
    for id in 3..=516 {
        let completed = tool_lifecycle_event(
            "read_file",
            &uuid::Uuid::from_u128(id).to_string(),
            "ToolCompleted",
            &ToolCallerLineageView::direct(),
            serde_json::json!({"succeeded":true,"preview":"done","subject":null}),
        );
        let mut completed = serde_json::to_value(event(turn, completed.kind().clone())).unwrap();
        completed["call_id"] = serde_json::json!(uuid::Uuid::from_u128(id));
        publish(
            &sender,
            observation,
            Arc::new(serde_json::from_value(completed).unwrap()),
        )
        .await;
    }
    let (snapshot, _) = observation.attach();
    let parent = uuid::Uuid::from_u128(2).to_string();
    assert!(snapshot.tools.iter().any(|op| matches!(op, HostEvent::ToolLine { call_id: Some(id), state, .. } if id == &parent && state == "running")));
    assert_eq!(snapshot.tools.len(), 512);
}

#[tokio::test]
async fn queued_follow_up_has_no_branch_sequence_gap_or_gap_qualified_rows() {
    use crate::runtime::child_live::tests::fixture_with;
    use plakous_offline_crash_harness::gate::GateHandle;
    let first = GateHandle::new();
    let next = GateHandle::new();
    let (_root, _sessions, attached, turn, _live) = fixture_with(serde_json::json!([
        {"output":{"text":"prefix"}},
        {"stream":[{"text":"first"},{"gate":first.script()}]},
        {"stream":[{"text":"queued"},{"gate":next.script()}]}
    ]))
    .await;
    first.entered().await;
    let agent = attached.agent();
    let submission = agent
        .follow_up_with_id(
            plakous::agent::CommandId::new("queued-observe").unwrap(),
            turn.id(),
            "queued",
        )
        .await
        .unwrap();
    let mut expected_events = submission.turn().unwrap().subscribe().unwrap();
    let mut admitted = ChildAdmission::new(submission, agent);
    let owner = admitted.projection.take().unwrap();
    first.release();
    next.entered().await;
    let mut expected_projection = TurnProjection::<Core>::default();
    let mut expected = Vec::new();
    loop {
        let event = expected_events.recv().await.unwrap();
        expected.extend(super::super::child_live::transcript_ops(
            &event,
            &mut expected_projection,
        ));
        if matches!(event.kind(), AgentEventKind::Model {event: Some(model), ..} if matches!(model.as_ref(), ModelEvent::TextDelta(_)))
        {
            break;
        }
    }
    observed(&owner.observation, |state| {
        state.progress.activity.as_deref() == Some("writing")
    })
    .await;
    {
        let state = owner.observation.lock();
        assert!(!state.projection.as_ref().unwrap().gap_observed);
        assert_eq!(
            state
                .updates
                .iter()
                .flat_map(|update| update.ops.iter().cloned())
                .collect::<Vec<_>>(),
            expected,
            "queued child follow-up observations must match the same event projection without gaps"
        );
        assert!(!state.updates.iter().flat_map(|update| update.ops.iter()).any(|op|
        matches!(op, HostEvent::Row(row) if row.text().is_some_and(|text| text.contains(super::super::child_live::GAP)))));
    }
    next.release();
    turn.result().await.unwrap();
    admitted.result.unwrap().result().await.unwrap();
    attached.release().await.unwrap();
}

#[tokio::test]
async fn structural_source_lag_reattaches_without_closing_the_observation() {
    use crate::runtime::child_live::tests::fixture_with;
    use plakous_offline_crash_harness::gate::GateHandle;
    let gate = GateHandle::new();
    let next = GateHandle::new();
    let after = GateHandle::new();
    let (_root, _sessions, attached, turn, _live) = fixture_with(serde_json::json!([
        {"output":{"text":"prefix"}},
        {"stream":[{"gate":gate.script()}],"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
        {"stream":[{"text":"before-resume"},{"gate":next.script()},{"text":"after"},{"text":"later"},{"gate":after.script()}]}
    ])).await;
    gate.entered().await;
    // Structural debt exceeds this subscription before the owner begins polling.
    let (_, initial) = attached
        .agent()
        .attach(plakous::agent::ViewRequest::new(
            plakous::agent::EventVisibility::Content,
            plakous::agent::SubscriptionLimits::new(1, 65536).unwrap(),
        ))
        .await
        .unwrap()
        .into_parts();
    gate.release();
    next.entered().await;
    let owner = ChildTurnProjection::start(
        turn.id(),
        Some(attached.agent().clone()),
        std::future::ready(Some(Source::Recovered(initial))),
    );
    // A successful reattach registers a fresh subscription after the backoff.
    observed(&owner.observation, |state| {
        state
            .projection
            .as_ref()
            .is_some_and(|projection| projection.gap_observed)
    })
    .await;
    assert!(!owner.observation.snapshot().terminal);
    next.release();
    after.entered().await;
    observed(&owner.observation, |state| {
        state
            .updates
            .iter()
            .flat_map(|update| update.ops.iter())
            .any(|op| matches!(op, HostEvent::AssistantDelta(text) if text == "later"))
    })
    .await;
    assert!(!owner.observation.snapshot().terminal);
    assert!(
        owner
            .observation
            .lock()
            .projection
            .as_ref()
            .unwrap()
            .gap_observed
    );
    let gaps = owner.observation.lock().updates.iter().flat_map(|update| update.ops.iter()).filter(|op| matches!(op, HostEvent::Row(row) if row.text() == Some(super::super::child_live::GAP))).count();
    assert_eq!(gaps, 1);
    let later = owner
        .observation
        .lock()
        .updates
        .iter()
        .find(|update| {
            update
                .ops
                .iter()
                .any(|op| matches!(op, HostEvent::AssistantDelta(text) if text == "later"))
        })
        .unwrap()
        .ops
        .clone();
    assert_eq!(later.as_ref(), &[HostEvent::AssistantDelta("later".into())]);
    after.release();
    turn.result().await.unwrap();
    attached.release().await.unwrap();
}

#[tokio::test]
async fn recovered_terminal_turn_closes_without_waiting_for_another_event() {
    use crate::runtime::child_live::tests::fixture_with;
    let (_root, _sessions, attached, turn, _live) = fixture_with(serde_json::json!([
        {"output":{"text":"prefix"}}, {"output":{"text":"done"}}
    ]))
    .await;
    let id = turn.id();
    turn.result().await.unwrap();
    let owner = ChildTurnProjection::recovered(Some(attached.agent().clone()), id);
    observed(&owner.observation, |state| state.closed).await;
    assert!(owner.observation.snapshot().terminal);
    attached.release().await.unwrap();
}

#[tokio::test]
async fn a_reduced_commit_between_the_two_attaches_is_delivered_once_without_a_gap() {
    use crate::runtime::child_live::{
        hooks,
        tests::{fixture_with, next_event, start_attach_probe},
    };
    use plakous_offline_crash_harness::gate::GateHandle;
    let commit = Arc::new(GateHandle::new());
    let finish = Arc::new(GateHandle::new());
    let (_root, sessions, attached, turn, _live) = fixture_with(serde_json::json!([
        {"output":{"text":"prefix"}},
        {"stream":[{"gate":commit.script()}],"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
        {"stream":[{"gate":finish.script()}]}
    ])).await;
    commit.entered().await;
    // Isolate the attach seam: report the committed exchange through the reducer,
    // without unrelated model/tool activity that already warrants a gap. Its
    // actual canonical rows still come from the live agent's history attachment.
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let owner = ChildTurnProjection::start(
        turn.id(),
        None,
        std::future::ready(Some(Source::Fixture(receiver))),
    );
    publish(
        &sender,
        &owner.observation,
        event(turn.id(), AgentEventKind::TurnStarted),
    )
    .await;
    let observation = owner.observation.clone();
    let updates = sender.clone();
    let completed = finish.clone();
    let committed = event(
        turn.id(),
        AgentEventKind::ConversationCommitted {
            item_count: 2,
            items: None,
        },
    );
    hooks::set_before_attach(
        "attach-order",
        Arc::new(move || {
            let observation = observation.clone();
            let updates = updates.clone();
            let commit = commit.clone();
            let completed = completed.clone();
            let committed = committed.clone();
            Box::pin(async move {
                commit.release();
                completed.entered().await;
                publish(&updates, &observation, committed).await;
            })
        }),
    );
    let following = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let entered = following.clone();
    let resumed = resume.clone();
    hooks::set_before_follow(
        "attach-order",
        Arc::new(move || {
            let entered = entered.clone();
            let resumed = resumed.clone();
            Box::pin(async move {
                entered.notify_one();
                resumed.notified().await;
            })
        }),
    );
    let (observers, face_events) =
        start_attach_probe(sessions, &attached, turn.id(), owner.observation.clone());
    tokio::time::timeout(Duration::from_secs(10), following.notified())
        .await
        .unwrap();
    let first = next_event(&face_events).await;
    let HostEvent::PresentationSnapshot(snapshot) = first else {
        panic!("expected committed baseline, got {first:?}")
    };
    assert_eq!(
        snapshot
            .items
            .iter()
            .filter(|item| item
                .row
                .as_tool()
                .is_some_and(|tool| tool.name == "loopback_effect"))
            .count(),
        1
    );
    assert!(
        !snapshot
            .items
            .iter()
            .any(|item| item.row.text() == Some(super::super::child_live::GAP))
    );
    sender.send(Received::Closed).await.unwrap();
    observed(&owner.observation, |state| state.closed).await;
    resume.notify_one();
    // The next publication is the converged canonical refresh, not a duplicate
    // tool row or a gap notice from the between-attaches commit.
    let last = next_event(&face_events).await;
    let HostEvent::TranscriptPatch(patch) = last else {
        panic!("expected canonical refresh, got {last:?}")
    };
    assert!(patch.upserts.is_empty(), "{patch:?}");
    assert!(face_events.try_iter().next().is_none());
    finish.release();
    turn.result().await.unwrap();
    drop(observers);
    attached.release().await.unwrap();
}

#[tokio::test]
async fn explicit_droppable_loss_marks_one_gap_without_retiring_the_child() {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(7)).unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let owner = ChildTurnProjection::start(
        turn,
        None,
        std::future::ready(Some(Source::Fixture(receiver))),
    );
    sender.send(Received::Lost).await.unwrap();
    sender.send(Received::Lost).await.unwrap();
    publish(
        &sender,
        &owner.observation,
        event(
            turn,
            AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(
                OutputLocation::new(0, 0),
                b"after loss".to_vec(),
            ))),
        ),
    )
    .await;
    {
        let state = owner.observation.lock();
        assert!(!state.closed);
        assert!(state.projection.as_ref().unwrap().gap_observed);
        assert_eq!(
            state.updates.back().unwrap().ops.as_ref(),
            &[
                HostEvent::Row(crate::sessions::TranscriptRow::info(
                    crate::sessions::NoticeScope::Tree,
                    super::super::child_live::GAP
                )),
                HostEvent::AssistantDelta("after loss".into()),
            ]
        );
    }
    publish(
        &sender,
        &owner.observation,
        event(
            turn,
            AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(
                OutputLocation::new(0, 0),
                b"later".to_vec(),
            ))),
        ),
    )
    .await;
    let state = owner.observation.lock();
    assert_eq!(state.updates.iter().flat_map(|update| update.ops.iter()).filter(|op| matches!(op, HostEvent::Row(row) if row.text() == Some(super::super::child_live::GAP))).count(), 1);
    assert_eq!(
        state.updates.back().unwrap().ops.as_ref(),
        &[HostEvent::AssistantDelta("later".into())]
    );
}

#[tokio::test]
async fn failed_admission_receiver_recovers_without_losing_the_admitted_result() {
    use crate::runtime::child_live::tests::fixture_with;
    use plakous_offline_crash_harness::gate::GateHandle;
    let gate = GateHandle::new();
    let (_root, _sessions, attached, mut turn, _live) = fixture_with(serde_json::json!([
        {"output":{"text":"prefix"}}, {"stream":[{"gate":gate.script()}], "output":{"text":"done"}}
    ]))
    .await;
    gate.entered().await;
    let mut held = Vec::new();
    while let Ok(attachment) = attached
        .agent()
        .attach(plakous::agent::ViewRequest::content())
        .await
    {
        held.push(attachment);
    }
    // The original receiver was taken by the first owner; a second take must
    // now fail subscriber admission. Presentation still cannot reject the turn.
    let fallback = ChildTurnProjection::admitted(&mut turn, attached.agent());
    assert!(!fallback.observation.snapshot().terminal);
    drop(held);
    gate.release();
    assert_eq!(
        turn.result()
            .await
            .unwrap()
            .response()
            .unwrap()
            .output_text(),
        "done"
    );
    observed(&fallback.observation, |state| state.closed).await;
    attached.release().await.unwrap();
}

#[tokio::test]
async fn fixed_burst_face_lag_resyncs_once_and_follows_the_terminal() {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(7)).unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let owner = ChildTurnProjection::start(
        turn,
        None,
        std::future::ready(Some(Source::Fixture(receiver))),
    );
    let observation = owner.observation.clone();
    let (_, mut tail) = observation.attach();
    for _ in 0..8 {
        publish(
            &sender,
            &observation,
            event(
                turn,
                AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(
                    OutputLocation::new(0, 0),
                    b"within-window".to_vec(),
                ))),
            ),
        )
        .await;
    }
    // This fixed ordinary burst must fit the promised 128-update face window.
    for _ in 0..8 {
        assert!(tail.recv().await.unwrap().is_some());
    }
    for _ in 0..256 {
        publish(
            &sender,
            &observation,
            event(
                turn,
                AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(
                    OutputLocation::new(0, 0),
                    b"evicted".to_vec(),
                ))),
            ),
        )
        .await;
    }
    let (emitted, mut face_events) = tokio::sync::mpsc::unbounded_channel();
    let followed = observation.clone();
    let task = tokio::spawn(async move {
        super::super::child_live::follow_turn(
            followed,
            tail,
            0,
            HashSet::new(),
            false,
            move |op| emitted.send(op).is_ok(),
        )
        .await;
    });
    let gap = tokio::time::timeout(Duration::from_secs(10), face_events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(gap, HostEvent::Row(row) if row.text() == Some(super::super::child_live::GAP))
    );
    publish(
        &sender,
        &observation,
        event(
            turn,
            AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(
                OutputLocation::new(0, 0),
                b"after-resync".to_vec(),
            ))),
        ),
    )
    .await;
    assert_eq!(
        face_events.recv().await.unwrap(),
        HostEvent::AssistantDelta("after-resync".into())
    );
    publish(
        &sender,
        &observation,
        event(
            turn,
            AgentEventKind::TurnCompleted {
                usage: plakous::agent::TurnUsage::default(),
            },
        ),
    )
    .await;
    task.await.unwrap();
    let rest = std::iter::from_fn(|| face_events.try_recv().ok()).collect::<Vec<_>>();
    assert!(rest.contains(&HostEvent::AssistantDone));
    assert!(!rest.iter().any(
        |op| matches!(op, HostEvent::Row(row) if row.text() == Some(super::super::child_live::GAP))
    ));
    assert!(observation.snapshot().terminal);
}
