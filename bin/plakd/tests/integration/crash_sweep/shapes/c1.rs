use super::*;

pub(in super::super) const CAPACITY: &str = "crash-sweep-capacity-denied";
pub(in super::super) const QUEUED: &str = "crash-sweep-queued-answer";
pub(in super::super) const QUEUED_PROMPT: &str = "crash-sweep-queued-prompt";

pub(super) fn hosted_tool_call_final() -> Value {
    let mut script = direct_tool_then_final();
    script["hosted_tool_call_final"] = json!(true);
    script
}

pub(super) fn assert_hosted(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    // The views count the provider-hosted call beside the local one: its
    // result block is a tool result of a `HistoryAppended` commit (spec 02).
    assert_live(fixture, outcome, state, 2, 2);
    let responses = plakous_offline_crash_harness::ledger_signature::inspect_provider_responses(
        &summary::paths(fixture).runtime_database(),
        state.head.parse().unwrap(),
    )
    .unwrap();
    let final_response = responses.last().expect("committed hosted final");
    let blocks = final_response.output()[0].blocks();
    assert!(
        matches!(&blocks[0], plakous::model::ContentBlock::ToolCall(call)
        if matches!(call.owner(), plakous::model::ToolExecutionOwner::ProviderHosted { .. })
        && call.state() == plakous::model::ToolCallState::Completed)
    );
    assert!(
        matches!(&blocks[1], plakous::model::ContentBlock::ToolResult(result)
        if matches!(result.owner(), plakous::model::ToolExecutionOwner::ProviderHosted { .. }))
    );
    assert!(
        matches!(&blocks[2], plakous::model::ContentBlock::Text(text) if text.text() == ANSWER)
    );
    assert_eq!(
        final_response.finish().reason(),
        plakous::model::FinishReason::Stop
    );
    assert_eq!(
        invariants::dispatches(fixture)
            .iter()
            .filter(|(kind, _)| kind == "EffectDispatched")
            .count(),
        1,
        "hosted call must not dispatch locally"
    );
}

pub(super) fn capacity_overflow() -> Value {
    json!({"version": 1, "steps": [
        {"fault": {"at": "pre_stream",
            "kind": "context_overflow", "message": CAPACITY}},
        {"output": {"text": QUEUED}}
    ]})
}

pub(in super::super) fn capacity_release_file(fixture: &Fixture) -> std::path::PathBuf {
    fixture.root.join("capacity-provider-release")
}

pub(super) fn configure_capacity(fixture: &Fixture) {
    let path = fixture.home().join("config.toml");
    let config = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path, config.replace("enabled = true", "enabled = false")).unwrap();
}

pub(super) fn capacity_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    if state.sent
        && state.stage == 0
        && invariants::dispatches(fixture)
            .iter()
            .any(|(kind, _)| kind == "ProviderDispatched")
    {
        state.stage = 1;
        return Some(prompt_frame(QUEUED_PROMPT));
    }
    if state.stage == 1
        && records.iter().any(|row| {
            row["kind"] == "queue"
                && row["items"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["text"] == QUEUED_PROMPT))
        })
        // Observe the indexed lifecycle as-is; projecting here would change
        // which commits the traced host makes before the crash cut.
        && summary::read(fixture).queued_prompt_pending
    {
        std::fs::write(capacity_release_file(fixture), b"release").unwrap();
        state.stage = 2;
    }
    if state.stage == 2
        && records.iter().any(|row| {
            row["kind"] == "error"
                && row["message"]
                    .as_str()
                    .is_some_and(|text| text.contains(CAPACITY))
        })
        && records.iter().any(|row| {
            row["kind"] == "queue"
                && row["drain"] == "restored_paused"
                && row["items"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
        })
    {
        let durable = summary::read(fixture);
        // JSONL observations can precede their transcript projection commit.
        if durable.capacity_terminals == 0 {
            return None;
        }
        assert_eq!(
            durable.capacity_terminals, 1,
            "capacity publication precedes resume"
        );
        assert_eq!(durable.drain, "restored_paused");
        assert_eq!(
            durable.queue.len(),
            1,
            "pilot queued prompt survived denial"
        );
        assert_eq!(
            invariants::dispatches(fixture).len(),
            1,
            "queued work passed capacity pause"
        );
        state.stage = 3;
        return Some(json!({"v": 1, "kind": "resume_queue"}));
    }
    if state.stage < 3 && state.sent {
        return None;
    }
    // Capacity additionally waits for the drained queue; the shared lifecycle
    // waits for the face's post-answer busy=false before sending quit.
    if state.stage == 3 {
        let done = records
            .iter()
            .rposition(|row| row["kind"] == "assistant_done")?;
        if !records[done + 1..].iter().any(|row| {
            row["kind"] == "queue"
                && row["drain"] == "live_auto_drain"
                && row["items"].as_array().is_some_and(Vec::is_empty)
        }) {
            return None;
        }
    }
    prompt_then_quit(fixture, records, state)
}

pub(super) fn assert_capacity(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_eq!(outcome.assistant_text(), [QUEUED]);
    assert_eq!(state.capacity_terminals, 1);
    assert_eq!(state.queued_answers, 1);
    assert_eq!(state.original_answers, 0);
    assert_eq!(state.tool_call_count, 0);
    assert_eq!(state.drain, "live_auto_drain");
    assert_eq!(state.queue, [] as [String; 0]);
    assert_eq!(invariants::dispatches(fixture).len(), 2);
    invariants::no_duplicate_dispatch(fixture);
}

#[test]
fn capacity_fault_waits_for_queue_before_release() {
    let script = capacity_overflow();
    assert_eq!(script["steps"][0]["fault"]["at"], "pre_stream");
    assert!(script["steps"][0]["fault"]["stall_ms"].is_null());
    let fixture = Fixture::new(&script);
    let mut state = Lifecycle {
        sent: true,
        stage: 1,
        ..Lifecycle::default()
    };
    for records in [
        vec![],
        vec![json!({"kind": "queue", "items": []})],
        vec![json!({"kind": "queue", "items": [{"text": "another prompt"}]})],
    ] {
        assert_eq!(capacity_lifecycle(&fixture, &records, &mut state), None);
        assert_eq!(state.stage, 1);
        assert!(!capacity_release_file(&fixture).exists());
    }
}

#[test]
fn capacity_quit_waits_for_post_answer_queue_snapshot() {
    let fixture = Fixture::new(&capacity_overflow());
    let done = json!({"kind": "assistant_done"});
    let empty = json!({"kind": "queue", "drain": "live_auto_drain", "items": []});
    for records in [
        vec![done.clone()],
        vec![done.clone(), empty.clone()],
        vec![empty.clone(), done.clone()],
        vec![done.clone(), json!({"kind": "status", "phase": "idle"})],
        vec![done.clone(), json!({"kind": "queue", "items": [{}]})],
        vec![
            done.clone(),
            json!({"kind": "queue", "drain": "restored_paused", "items": []}),
        ],
    ] {
        let mut state = Lifecycle {
            sent: true,
            stage: 3,
            ..Lifecycle::default()
        };
        assert_eq!(capacity_lifecycle(&fixture, &records, &mut state), None);
        assert!(!state.quit);
    }
    let mut state = Lifecycle {
        sent: true,
        stage: 3,
        ..Lifecycle::default()
    };
    assert_eq!(
        capacity_lifecycle(
            &fixture,
            &[done, json!({"kind": "busy", "busy": false}), empty],
            &mut state
        ),
        Some(quit_frame())
    );
    assert!(state.quit);
}
