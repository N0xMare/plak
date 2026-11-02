use super::*;
use crate::runtime::child_live::{covered_tool_calls, emit_retained_tools, follow_turn};
use plakous_host_protocol::views::presentation::{ActivityState, PresentationSnapshot};
use plakous_host_protocol::views::transcript::{
    Lane, RowKey, TranscriptItem, TranscriptScope, TranscriptState,
};

async fn tool(
    sender: &tokio::sync::mpsc::Sender<Received>,
    observation: &ChildObservation,
    turn: TurnId,
    label: &str,
) {
    let lifecycle = tool_lifecycle_event(
        "read_file",
        &uuid::Uuid::from_u128(2).to_string(),
        label,
        &ToolCallerLineageView::direct(),
        if label == "ToolStarted" {
            serde_json::json!({"arguments_preview":"{}","subject":"file"})
        } else {
            serde_json::json!({"succeeded":true,"preview":"done","subject":null})
        },
    );
    publish(sender, observation, event(turn, lifecycle.kind().clone())).await;
}

fn fixture() -> (
    TurnId,
    tokio::sync::mpsc::Sender<Received>,
    ChildTurnProjection,
) {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0xfa)).unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    (
        turn,
        sender,
        ChildTurnProjection::start(
            &crate::TaskSet::new(),
            turn,
            None,
            std::future::ready(Some(Source::Fixture(receiver))),
        ),
    )
}

fn baseline(state: &str) -> Vec<TranscriptItem> {
    vec![TranscriptItem {key: RowKey::new(77, Lane::Runtime, 0), row: serde_json::from_value(serde_json::json!({
        "kind":"tool", "call_id":uuid::Uuid::from_u128(2).to_string(), "name":"read_file", "activity_kind":"tool", "state":state
    })).unwrap()}]
}

fn apply(events: &crate::surface_protocol::HostEventReceiver, state: &mut TranscriptState) {
    for event in events.try_iter() {
        match event {
            HostEvent::PresentationSnapshot(snapshot) => state.replace(snapshot).unwrap(),
            HostEvent::TranscriptPatch(patch) => state.apply(patch).unwrap(),
            _ => {}
        }
    }
}

fn assert_call(state: &TranscriptState, expected: ActivityState) -> RowKey {
    let rows: Vec<_> = state
        .items()
        .iter()
        .filter(|(_, row)| row.as_tool().is_some())
        .collect();
    assert_eq!(rows.len(), 1, "one retained call, not a second attach row");
    assert_eq!(rows[0].1.as_tool().unwrap().activity_state(), expected);
    *rows[0].0
}

#[tokio::test]
async fn committed_interrupted_call_is_overridden_running_and_settles_at_the_same_key() {
    interrupted_attach(false).await;
}

#[tokio::test]
async fn result_between_observation_attach_and_history_snapshot_settles_the_same_row() {
    interrupted_attach(true).await;
}

async fn interrupted_attach(result_during_history: bool) {
    let (turn, source, owner) = fixture();
    tool(&source, &owner.observation, turn, "ToolStarted").await;
    let (snapshot, tail) = owner.observation.attach();
    if result_during_history {
        tool(&source, &owner.observation, turn, "ToolCompleted").await;
    }
    let rows = baseline("interrupted");
    let expected_key = rows[0].key;
    let covered = covered_tool_calls(&rows);
    let (wire, events) = crate::surface_protocol::host_event_channel();
    let publisher: crate::runtime::Sender = crate::runtime::Sender::over(wire);
    publisher.replace_transcript(PresentationSnapshot {
        scope: TranscriptScope::Agent("child".into()),
        items: rows,
        ..PresentationSnapshot::default()
    });
    let emit = |op| publisher.observe_agent("child", &op);
    emit_retained_tools(snapshot.tools, &covered, &emit);
    let mut state = TranscriptState::default();
    apply(&events, &mut state);
    assert_eq!(assert_call(&state, ActivityState::Running), expected_key);
    if !result_during_history {
        tool(&source, &owner.observation, turn, "ToolCompleted").await;
    }
    drop(source);
    observed(&owner.observation, |state| state.closed).await;
    follow_turn(
        owner.observation.clone(),
        tail,
        u64::MAX,
        covered,
        false,
        emit,
    )
    .await;
    apply(&events, &mut state);
    assert_eq!(assert_call(&state, ActivityState::Succeeded), expected_key);
}

#[tokio::test]
async fn settled_history_covers_a_stale_retained_running_call_at_the_same_key() {
    let (turn, source, owner) = fixture();
    tool(&source, &owner.observation, turn, "ToolStarted").await;
    let (snapshot, _) = owner.observation.attach();
    let rows = baseline("succeeded");
    let key = rows[0].key;
    let covered = covered_tool_calls(&rows);
    let (wire, events) = crate::surface_protocol::host_event_channel();
    let publisher: crate::runtime::Sender = crate::runtime::Sender::over(wire);
    publisher.replace_transcript(PresentationSnapshot {
        scope: TranscriptScope::Agent("child".into()),
        items: rows,
        ..PresentationSnapshot::default()
    });
    emit_retained_tools(snapshot.tools, &covered, &|op| {
        publisher.observe_agent("child", &op)
    });
    let mut state = TranscriptState::default();
    apply(&events, &mut state);
    assert_eq!(assert_call(&state, ActivityState::Succeeded), key);
}

#[tokio::test]
async fn lagged_face_reattaches_running_call_then_settles_the_same_row_id() {
    let (turn, source, owner) = fixture();
    tool(&source, &owner.observation, turn, "ToolStarted").await;
    let (snapshot, tail) = owner.observation.attach();
    let (wire, events) = crate::surface_protocol::host_event_channel();
    let publisher: crate::runtime::Sender = crate::runtime::Sender::over(wire);
    publisher.replace_transcript(PresentationSnapshot {
        scope: TranscriptScope::Agent("child".into()),
        ..PresentationSnapshot::default()
    });
    let mut state = TranscriptState::default();
    emit_retained_tools(snapshot.tools, &HashSet::new(), &|op| {
        publisher.observe_agent("child", &op)
    });
    apply(&events, &mut state);
    let key = assert_call(&state, ActivityState::Running);
    for _ in 0..=RETAINED_UPDATES {
        publish(
            &source,
            &owner.observation,
            event(turn, AgentEventKind::ModelDispatchStarted),
        )
        .await;
    }
    let (notified, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(follow_turn(
        owner.observation.clone(),
        tail,
        0,
        HashSet::new(),
        false,
        move |op| {
            let sent = publisher.observe_agent("child", &op);
            notified.send(op).unwrap();
            sent
        },
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop { if matches!(notifications.recv().await, Some(HostEvent::ToolLine {state, ..}) if state == "running") {break;} }
    }).await.expect("reattach re-emits retained running observation, not only the gap");
    apply(&events, &mut state);
    assert_eq!(assert_call(&state, ActivityState::Running), key);
    tool(&source, &owner.observation, turn, "ToolCompleted").await;
    drop(source);
    task.await.unwrap();
    apply(&events, &mut state);
    assert_eq!(assert_call(&state, ActivityState::Succeeded), key);
}

#[tokio::test]
async fn start_and_completion_of_one_call_occupy_one_retained_slot() {
    let (turn, source, owner) = fixture();
    tool(&source, &owner.observation, turn, "ToolStarted").await;
    tool(&source, &owner.observation, turn, "ToolCompleted").await;
    let (snapshot, _) = owner.observation.attach();
    assert_eq!(snapshot.tools.len(), 1);
    assert!(matches!(&snapshot.tools[0], HostEvent::ToolLine {state, ..} if state == "done"));
}
