use super::super::offline_support::{
    AgentOutcome, AgentProcess, Fixture, prompt_frame, quit_frame,
};
use super::{invariants, summary};
use plak::store::session_records::SessionRecordStore as _;
use serde_json::{Value, json};
pub(super) mod c1;
mod sw3a;

#[test]
fn uncertain_mcp_tool_pilot_proves_live_shape() {
    let shape = "uncertain_mcp_tool";
    let (seed, session) = super::driver::setup(shape);
    super::driver::pilot(&seed, &session, shape);
}

#[test]
fn hosted_tool_call_final_pilot_proves_live_shape() {
    let shape = "hosted_tool_call_final";
    let (seed, session) = super::driver::setup(shape);
    super::driver::pilot(&seed, &session, shape);
}

#[test]
fn capacity_overflow_pilot_proves_live_shape() {
    let shape = "capacity_overflow";
    let (seed, session) = super::driver::setup(shape);
    super::driver::pilot(&seed, &session, shape);
}

#[test]
fn final_only_pilot_waits_for_face_idle() {
    let shape = "final_only";
    let (seed, session) = super::driver::setup(shape);
    super::driver::pilot(&seed, &session, shape);
}

#[test]
fn busy_quit_escalates_pilot_proves_deaf_stall_and_identical_twins() {
    let shape = "busy_quit_escalates";
    let (seed, session) = super::driver::setup(shape);
    super::driver::pilot(&seed, &session, shape);
}

#[test]
fn busy_quit_lifecycle_waits_for_its_running_provider_prefix() {
    let fixture = Fixture::new(&busy_quit_script());
    let mut state = Lifecycle {
        sent: true,
        ..Lifecycle::default()
    };
    for records in [
        vec![],
        vec![json!({"kind": "busy", "busy": true})],
        vec![json!({"kind": "assistant", "text": "another prefix"})],
    ] {
        assert_eq!(busy_quit_lifecycle(&fixture, &records, &mut state), None);
    }
    let records = vec![
        json!({"kind": "busy", "busy": true}),
        json!({"kind": "assistant", "text": BUSY_QUIT_PREFIX}),
    ];
    assert_eq!(
        busy_quit_lifecycle(&fixture, &records, &mut state),
        Some(quit_frame())
    );
    assert_eq!(busy_quit_lifecycle(&fixture, &records, &mut state), None);
}

#[test]
fn terminal_expectation_distinguishes_capacity_from_an_answer() {
    let state = summary::Summary {
        capacity_terminals: 1,
        ..summary::Summary::default()
    };
    assert_eq!(get("capacity_overflow").published(&state), 1);
    assert_eq!(get("final_only").published(&state), 0);
    assert!(!get("capacity_overflow").subset);
    assert!(!get("hosted_tool_call_final").subset);
}

pub(super) const ANSWER: &str = "crash-sweep-original-answer";
pub(super) const FOLLOWUP: &str = "crash-sweep-followup-answer";
pub(super) struct Shape {
    pub name: &'static str,
    pub subset: bool,
    pub terminal: ExpectedTerminal,
    pub script: fn() -> Value,
    pub configure: fn(&Fixture),
    pub setup: fn(&mut AgentProcess),
    pub lifecycle: fn(&Fixture, &[Value], &mut Lifecycle) -> Option<Value>,
    pub assert_live: fn(&Fixture, &AgentOutcome, &summary::Summary),
}

impl Shape {
    pub(super) fn published(&self, state: &summary::Summary) -> usize {
        match self.terminal {
            ExpectedTerminal::Answer => state.original_answers,
            ExpectedTerminal::CapacityDenied => state.capacity_terminals,
        }
    }

    pub(super) fn followup(&self) -> Value {
        let mut script = (self.script)();
        script["steps"] = json!([{"output": {"text": FOLLOWUP}, "finish": "stop"}]);
        script
    }
}

#[derive(Default)]
pub(super) struct Lifecycle {
    pub sent: bool,
    pub quit: bool,
    pub stage: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExpectedTerminal {
    Answer,
    CapacityDenied,
}

pub(super) const REGISTRY: &[Shape] = &[
    Shape {
        name: "uncertain_mcp_tool",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: uncertain_mcp_script,
        configure: configure_uncertain_mcp,
        setup: no_setup,
        lifecycle: uncertain_mcp_lifecycle,
        assert_live: assert_uncertain_mcp,
    },
    Shape {
        name: "busy_quit_escalates",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: busy_quit_script,
        configure: no_configure,
        setup: no_setup,
        lifecycle: busy_quit_lifecycle,
        assert_live: assert_busy_quit,
    },
    Shape {
        name: "hosted_tool_call_final",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: c1::hosted_tool_call_final,
        configure: no_configure,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: c1::assert_hosted,
    },
    Shape {
        name: "capacity_overflow",
        subset: false,
        terminal: ExpectedTerminal::CapacityDenied,
        script: c1::capacity_overflow,
        configure: c1::configure_capacity,
        setup: no_setup,
        lifecycle: c1::capacity_lifecycle,
        assert_live: c1::assert_capacity,
    },
    Shape {
        name: "final_only",
        subset: true,
        terminal: ExpectedTerminal::Answer,
        script: final_only,
        configure: no_configure,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: assert_final_only,
    },
    Shape {
        name: "direct_tool_then_final",
        subset: true,
        terminal: ExpectedTerminal::Answer,
        script: direct_tool_then_final,
        configure: no_configure,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: assert_direct_tool,
    },
    Shape {
        name: "promoted_background_job",
        subset: true,
        terminal: ExpectedTerminal::Answer,
        script: sw3a::promoted_background_job,
        configure: sw3a::configure_process,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: sw3a::assert_process,
    },
    Shape {
        name: "sealed_family_artifacts",
        subset: true,
        terminal: ExpectedTerminal::Answer,
        script: sw3a::sealed_family_artifacts,
        configure: sw3a::configure_process,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: sw3a::assert_sealed,
    },
    Shape {
        name: "non_stop_finish_length",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: non_stop_length,
        configure: no_configure,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: assert_length,
    },
    Shape {
        name: "non_stop_finish_refusal",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: non_stop_refusal,
        configure: no_configure,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: assert_refusal,
    },
    Shape {
        name: "steer_accepted_mid_turn",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: steer_script,
        configure: no_configure,
        setup: no_setup,
        lifecycle: steer_lifecycle,
        assert_live: assert_steer,
    },
    Shape {
        name: "scheduled_compaction_in_turn",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: in_turn_compaction,
        configure: no_configure,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: assert_in_turn_compaction,
    },
    Shape {
        name: "scheduled_compaction_workflow",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: workflow_compaction,
        configure: configure_workflow,
        setup: setup_workflow,
        lifecycle: workflow_lifecycle,
        assert_live: assert_workflow_compaction,
    },
    Shape {
        name: "code_mode_cell_nested_then_final",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: nested_cell,
        configure: no_configure,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: assert_nested_cell,
    },
    Shape {
        name: "code_mode_cell_final",
        subset: false,
        terminal: ExpectedTerminal::Answer,
        script: final_cell,
        configure: no_configure,
        setup: no_setup,
        lifecycle: prompt_then_quit,
        assert_live: assert_final_cell,
    },
];

pub(super) fn get(name: &str) -> &'static Shape {
    REGISTRY
        .iter()
        .find(|shape| shape.name == name)
        .expect("registered shape")
}

pub(super) fn names(subset: bool) -> Vec<&'static str> {
    REGISTRY
        .iter()
        .filter(|shape| !subset || shape.subset)
        .map(|shape| shape.name)
        .collect()
}

fn no_setup(_: &mut AgentProcess) {}

fn no_configure(_: &Fixture) {}

const BUSY_QUIT_PREFIX: &str = "crash-sweep-busy-quit-prefix";

fn busy_quit_script() -> Value {
    json!({
        "version": 1,
        "stall_ignores_cancellation": true,
        "steps": [{"stream": [{"text": BUSY_QUIT_PREFIX}, {"stall": 30000}]}]
    })
}

fn busy_quit_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    if state.sent
        && !state.quit
        && records.iter().any(|row| {
            row["kind"] == "assistant"
                && row["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(BUSY_QUIT_PREFIX))
        })
    {
        assert!(
            records
                .iter()
                .rfind(|row| row["kind"] == "busy")
                .is_some_and(|row| row["busy"] == true),
            "the deaf stall must still be busy at quit"
        );
        state.quit = true;
        return Some(quit_frame());
    }
    // A not-yet-admitted prompt can be requeued at a cut and complete on resume.
    prompt_then_quit(fixture, records, state)
}

fn assert_busy_quit(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert!(
        outcome
            .assistant_text()
            .iter()
            .any(|text| text.contains(BUSY_QUIT_PREFIX))
    );
    assert!(
        outcome.lines.iter().any(|line| line
            .contains("quit escalated a cancel that did not settle")
            && line.contains("300ms")),
        "quit did not exhaust its cancellation grace"
    );
    assert_eq!(
        state.runtime_verdict, "RecoversAbandonedTurn",
        "deaf quit synthesized a terminal instead of releasing only the lease"
    );
    assert!(state.head_turn.is_some());
    assert!(state.phase.is_some());
    assert_eq!(state.original_answers, 0);
    assert_eq!(state.root_turn_count, 1);
    assert_eq!(invariants::dispatches(fixture).len(), 1);
    assert!(
        state.abandonment_claims.is_empty(),
        "abandonment is a reopen fact, not an invented live terminal"
    );
    let paths = summary::paths(fixture);
    let store = plak::store::session_records::SqliteSessionStore::new(paths);
    let record = store
        .list_for_workspace(&fixture.workspace())
        .unwrap()
        .pop()
        .unwrap();
    let views = store.load_views(&record.id).unwrap().unwrap();
    assert!(views.state.released && views.state.head_working);
    assert!(!views.state.open_turns.is_empty());
}

fn face_idle_after(records: &[Value], boundary: impl Fn(&Value) -> bool) -> bool {
    let Some(completed) = records.iter().rposition(boundary) else {
        return false;
    };
    // The answer/tool marker precedes busy=false. A stale idle observation
    // cannot authorize quit, which would inject an interrupt while face-busy.
    records[completed + 1..]
        .iter()
        .rfind(|row| row["kind"] == "busy")
        .is_some_and(|row| row["busy"] == false)
}

fn prompt_then_quit(_: &Fixture, records: &[Value], state: &mut Lifecycle) -> Option<Value> {
    if !state.sent && records.iter().any(|r| r["kind"] == "ready") {
        state.sent = true;
        Some(prompt_frame("crash-sweep-original-prompt"))
    } else if !state.quit && face_idle_after(records, |row| row["kind"] == "assistant_done") {
        state.quit = true;
        Some(quit_frame())
    } else {
        None
    }
}

#[test]
fn prompt_lifecycle_waits_for_post_answer_face_idle() {
    let fixture = Fixture::new(&final_only());
    let done = json!({"kind": "assistant_done"});
    let idle = json!({"kind": "busy", "busy": false});
    let busy = json!({"kind": "busy", "busy": true});
    for records in [
        vec![done.clone()],
        vec![idle.clone(), done.clone()],
        vec![done.clone(), json!({"kind": "status", "phase": "idle"})],
        vec![done.clone(), idle.clone(), busy],
    ] {
        let mut state = Lifecycle {
            sent: true,
            ..Lifecycle::default()
        };
        assert_eq!(prompt_then_quit(&fixture, &records, &mut state), None);
        assert!(!state.quit);
    }
    let mut state = Lifecycle {
        sent: true,
        ..Lifecycle::default()
    };
    assert_eq!(
        prompt_then_quit(&fixture, &[done, idle], &mut state),
        Some(quit_frame())
    );
    assert!(state.quit);
}

fn assert_final_only(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_live(fixture, outcome, state, 0, 1);
}

fn assert_direct_tool(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_live(fixture, outcome, state, 1, 2);
}

fn assert_live(
    fixture: &Fixture,
    outcome: &AgentOutcome,
    state: &summary::Summary,
    tools: i64,
    providers: usize,
) {
    assert_eq!(outcome.assistant_text(), [ANSWER]);
    assert_eq!(state.original_answers, 1, "pilot live publication");
    assert_eq!(state.tool_call_count, tools, "pilot live tool shape");
    assert_eq!(
        invariants::dispatches(fixture)
            .iter()
            .filter(|(kind, _)| kind == "ProviderDispatched")
            .count(),
        providers
    );
    invariants::no_duplicate_dispatch(fixture);
}

pub(super) fn script(shape: &str) -> Value {
    (get(shape).script)()
}

fn final_only() -> Value {
    basic_script(false)
}

fn direct_tool_then_final() -> Value {
    basic_script(true)
}

fn basic_script(tool: bool) -> Value {
    let final_step = json!({"output": {"text": ANSWER}, "finish": "stop"});
    let steps = if tool {
        vec![
            json!({"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"}),
            final_step,
        ]
    } else {
        vec![final_step]
    };
    json!({"version": 1, "steps": steps})
}

fn non_stop_length() -> Value {
    non_stop("length")
}

fn non_stop_refusal() -> Value {
    non_stop("refusal")
}

fn non_stop(finish: &str) -> Value {
    json!({"version": 1, "steps": [{"output": {"text": ANSWER}, "finish": finish}]})
}

fn assert_finish(
    fixture: &Fixture,
    outcome: &AgentOutcome,
    state: &summary::Summary,
    finish: plakous::model::FinishReason,
) {
    assert_final_only(fixture, outcome, state);
    let responses = plakous_offline_crash_harness::ledger_signature::inspect_provider_responses(
        &summary::paths(fixture).runtime_database(),
        state.head.parse().unwrap(),
    )
    .unwrap();
    assert_eq!(
        responses.last().unwrap().finish().reason(),
        finish,
        "pilot actual provider finish"
    );
}

fn assert_length(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_finish(
        fixture,
        outcome,
        state,
        plakous::model::FinishReason::Length,
    );
}

fn assert_refusal(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_finish(
        fixture,
        outcome,
        state,
        plakous::model::FinishReason::Refusal,
    );
}

fn steer_script() -> Value {
    json!({"version": 1, "steps": [
        {"output": {"text": "crash-sweep-steered-round"}, "stall_ms": 300},
        {"output": {"text": ANSWER}}
    ]})
}

fn steer_lifecycle(fixture: &Fixture, records: &[Value], state: &mut Lifecycle) -> Option<Value> {
    if state.sent
        && state.stage == 0
        && invariants::dispatches(fixture)
            .iter()
            .any(|(kind, _)| kind == "ProviderDispatched")
    {
        state.stage = 1;
        let mut steer = prompt_frame("crash-sweep-mid-turn-steer");
        steer["id"] = json!("crash-sweep-steer");
        steer["steer"] = json!(true);
        return Some(steer);
    }
    prompt_then_quit(fixture, records, state)
}

fn assert_steer(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_eq!(state.original_answers, 1);
    let records = outcome.records();
    assert_eq!(
        records
            .iter()
            .filter(|row| row["kind"] == "steer_ack" && row["state"] == "queued")
            .count(),
        1,
        "pilot accepted mid-turn steer"
    );
    assert_eq!(
        records
            .iter()
            .filter(|row| row["kind"] == "steer_ack" && row["state"] == "applied")
            .count(),
        1,
        "pilot applied steer"
    );
    assert_eq!(
        state
            .ledger
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "command" && row["state"] == "finalized")
            .count(),
        2,
        "pilot durable root and accepted steer"
    );
    assert_eq!(
        invariants::dispatches(fixture)
            .iter()
            .filter(|(kind, _)| kind == "ProviderDispatched")
            .count(),
        2
    );
    invariants::no_duplicate_dispatch(fixture);
}

fn in_turn_compaction() -> Value {
    json!({"version": 1, "repeat_last_step": true, "maximum_projected_input": 20_000,
    "scheduled_compaction": true, "content_sensitive_projection": true,
    "tool_result_bytes": 5_800, "fold": "accept", "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls", "usage": {}},
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls", "usage": {}},
            {"output": {"text": "crash-sweep-non-marker-summary"}, "usage": {}},
            {"output": {"text": ANSWER}, "usage": {}}
    ]})
}

fn assert_in_turn_compaction(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_live(fixture, outcome, state, 2, 4);
    assert!(
        state
            .ledger
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "compaction" && row["state"] == "finalized"),
        "pilot finalized in-turn compaction"
    );
    assert_compact_row(outcome);
}

fn assert_compact_row(outcome: &AgentOutcome) {
    assert!(
        outcome.records().iter().any(|row| row["kind"] == "tool"
            && row["activity_kind"] == "compact"
            && row["state"] == "done"),
        "pilot committed Compact row"
    );
}

fn workflow_compaction() -> Value {
    json!({"version": 1, "maximum_projected_input": 20_000,
    "scheduled_compaction": true, "content_sensitive_projection": true,
    "fold": "accept", "steps": [
        {"output": {"text": format!("crash-sweep-seed-one {}", "detail ".repeat(800))}},
        {"output": {"text": "crash-sweep-seed-two"}},
        {"output": {"text": ANSWER}}
    ]})
}

fn setup_workflow(agent: &mut AgentProcess) {
    agent.send(&prompt_frame("crash-sweep-foldable-seed-one"));
    agent.wait_for_completed_turns(1);
    agent.send(&prompt_frame("crash-sweep-foldable-seed-two"));
    agent.wait_for_completed_turns(2);
}

fn configure_workflow(fixture: &Fixture) {
    if !summary::paths(fixture).runtime_database().is_file() {
        return;
    }
    let current: Value = serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    fixture.write_script(&post_setup_workflow_script(current));
}

/// After setup only the seed plan advances to the original answer; a written
/// follow-up or requeued script is left as the driver wrote it.
fn post_setup_workflow_script(current: Value) -> Value {
    if current["steps"] != workflow_compaction()["steps"] {
        return current;
    }
    let mut plan = workflow_compaction();
    plan["steps"] = json!([{"output": {"text": ANSWER}}]);
    plan
}

#[test]
fn workflow_configure_rewrites_only_the_seed_plan() {
    let advanced = post_setup_workflow_script(workflow_compaction());
    assert_eq!(advanced["steps"], json!([{"output": {"text": ANSWER}}]));
    let mut followup = workflow_compaction();
    followup["steps"] = json!([{"output": {"text": FOLLOWUP}}]);
    followup["repeat_last_step"] = json!(true);
    assert_eq!(post_setup_workflow_script(followup.clone()), followup);
    assert_eq!(post_setup_workflow_script(advanced.clone()), advanced);
}

fn workflow_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    if state.sent
        && state.stage == 0
        && face_idle_after(records, |row| row["kind"] == "assistant_done")
    {
        state.stage = 1;
        return Some(json!({"v": 1, "kind": "compact"}));
    }
    if state.stage == 1
        && !face_idle_after(records, |row| {
            row["kind"] == "tool"
                && row["activity_kind"] == "compact"
                && matches!(row["state"].as_str(), Some("done" | "failed"))
        })
    {
        return None;
    }
    prompt_then_quit(fixture, records, state)
}

#[test]
fn workflow_lifecycle_waits_for_post_answer_face_idle() {
    let fixture = Fixture::new(&workflow_compaction());
    let done = json!({"kind": "assistant_done"});
    let idle = json!({"kind": "busy", "busy": false});
    let mut state = Lifecycle {
        sent: true,
        ..Lifecycle::default()
    };
    assert_eq!(
        workflow_lifecycle(&fixture, &[idle.clone(), done.clone()], &mut state),
        None
    );
    assert_eq!(state.stage, 0);
    assert_eq!(
        workflow_lifecycle(&fixture, &[done, idle], &mut state),
        Some(json!({"v": 1, "kind": "compact"}))
    );
    assert_eq!(state.stage, 1);
    assert!(!state.quit);
}

#[test]
fn workflow_lifecycle_waits_for_post_compact_face_idle() {
    let fixture = Fixture::new(&workflow_compaction());
    for terminal in ["done", "failed"] {
        let mut records = vec![
            json!({"kind": "assistant_done"}),
            json!({"kind": "busy", "busy": false}),
            json!({"kind": "busy", "busy": true}),
            json!({"kind": "tool", "activity_kind": "compact", "state": terminal}),
        ];
        let mut state = Lifecycle {
            sent: true,
            stage: 1,
            ..Lifecycle::default()
        };
        assert_eq!(workflow_lifecycle(&fixture, &records, &mut state), None);
        assert!(!state.quit);
        records.push(json!({"kind": "busy", "busy": false}));
        assert_eq!(
            workflow_lifecycle(&fixture, &records, &mut state),
            Some(quit_frame())
        );
        assert!(state.quit);
    }
}

fn assert_workflow_compaction(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_eq!(outcome.assistant_text(), [ANSWER]);
    assert_eq!(state.original_answers, 1);
    assert_compact_row(outcome);
    assert!(
        state
            .workflow_requests
            .iter()
            .any(|row| row["kind"] == "compact" && row["state"] == "settled"),
        "pilot product compaction workflow"
    );
    invariants::no_duplicate_dispatch(fixture);
}

fn cell_script(program: &str) -> Value {
    json!({"version": 1, "code_mode": true, "steps": [
        {"output": {"cell": program}, "finish": "tool_calls"},
        {"output": {"text": ANSWER}}
    ]})
}

fn nested_cell() -> Value {
    cell_script(
        "const result = await tools.exec_command({cmd: 'printf crash-sweep-nested'}); text(result);",
    )
}

fn final_cell() -> Value {
    cell_script("text('crash-sweep-cell-final');")
}

fn assert_cell(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary, nested: bool) {
    assert_eq!(outcome.assistant_text(), [ANSWER]);
    assert_eq!(state.original_answers, 1);
    let records = outcome.records();
    let cell = records
        .iter()
        .find(|row| row["kind"] == "tool" && row["name"] == "exec" && row["state"] == "done")
        .expect("pilot completed real JS cell");
    assert_eq!(
        records
            .iter()
            .filter(|row| row["kind"] == "tool"
                && row["parent_call_id"] == cell["call_id"]
                && row["state"] == "done")
            .count(),
        usize::from(nested),
        "pilot nested tool count"
    );
    assert!(
        state
            .ledger
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "composition_evaluation" && row["state"] == "finalized"),
        "pilot finalized Code Cell ledger"
    );
    invariants::no_duplicate_dispatch(fixture);
}

fn assert_nested_cell(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_cell(fixture, outcome, state, true);
}

fn assert_final_cell(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_cell(fixture, outcome, state, false);
}

#[test]
fn registry_preserves_subset_scripts() {
    assert_eq!(
        names(true),
        [
            "final_only",
            "direct_tool_then_final",
            "promoted_background_job",
            "sealed_family_artifacts"
        ]
    );
    for name in names(false) {
        let _: plakous_offline_crash_harness::script::ScriptPlan =
            serde_json::from_value(script(name)).unwrap();
    }
    assert_eq!(script("final_only")["steps"].as_array().unwrap().len(), 1);
    assert_eq!(
        script("direct_tool_then_final")["steps"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn followup_preserves_descriptor_and_script_dials() {
    fn dial_script() -> Value {
        json!({"version": 1, "code_mode": true, "scheduled_compaction": true,
            "maximum_projected_input": 20_000, "tool_result_bytes": 5_800,
            "steps": [{"output": {"cell": "text('cell');"}, "finish": "tool_calls"}]})
    }
    let shape = Shape {
        script: dial_script,
        ..REGISTRY[0]
    };
    let mut followup = shape.followup();
    assert_eq!(
        followup["steps"],
        json!([{"output": {"text": FOLLOWUP}, "finish": "stop"}])
    );
    let mut original = dial_script();
    original.as_object_mut().unwrap().remove("steps");
    followup.as_object_mut().unwrap().remove("steps");
    assert_eq!(original, followup);
}

#[test]
#[ignore = "opt-in live-fact pilots; requires the JavaScript sidecar"]
fn live_fact_pilots() {
    for name in [
        "non_stop_finish_length",
        "non_stop_finish_refusal",
        "steer_accepted_mid_turn",
        "scheduled_compaction_in_turn",
        "scheduled_compaction_workflow",
        "code_mode_cell_nested_then_final",
        "code_mode_cell_final",
    ] {
        eprintln!("live-fact pilot: {name}");
        assert!(!get(name).subset, "live-fact shapes must remain opt-in");
        let (seed, session) = super::driver::setup(name);
        let pilot = super::driver::pilot(&seed, &session, name);
        assert!(pilot.iter().any(|commit| commit.store == "runtime"));
    }
}

fn uncertain_mcp_script() -> Value {
    json!({"version":1,"steps":[{"output":{"tool":{"name":"mcp__fake__escaped_result","arguments":{}}},"finish":"tool_calls"}]})
}

fn configure_uncertain_mcp(fixture: &Fixture) {
    let config = std::fs::read_to_string(fixture.home().join("config.toml")).unwrap();
    if !config.contains("[mcp_servers.fake]") {
        super::super::offline_e2e::enable_fake_mcp(fixture, &[]);
    }
}

fn uncertain_mcp_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    if state.sent
        && !state.quit
        && records
            .iter()
            .any(|row| row["kind"] == "turn_lifecycle" && row["phase"] == "recovery_required")
    {
        state.quit = true;
        return Some(quit_frame());
    }
    prompt_then_quit(fixture, records, state)
}

fn assert_uncertain_mcp(_: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert!(
        outcome
            .records()
            .iter()
            .any(|row| row["kind"] == "turn_lifecycle" && row["phase"] == "recovery_required")
    );
    assert_eq!(state.tool_call_count, 1);
    assert_eq!(state.original_answers, 0);
}
