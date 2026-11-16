//! Small real-cell turns put retirement in the indexed store-cut campaign.

use super::*;

pub(super) fn script() -> Value {
    let mut steps = (0..6).map(|_| json!({"output":{"cell":"await tools.exec_command({cmd:'printf retired'}); text('closed');"},"finish":"tool_calls"})).collect::<Vec<_>>();
    steps.push(json!({"output":{"text":ANSWER}}));
    json!({"version":1,"code_mode":true,"steps":steps})
}

pub(super) fn configure(fixture: &Fixture) {
    fixture.set_ledger_retirement_high_water(8);
}

pub(super) fn interrupted_batch_script() -> Value {
    let call = json!({"name":"loopback_effect","arguments":{}});
    let mut steps = (0..6)
        .map(|_| json!({"output":{"tool":call},"finish":"tool_calls"}))
        .collect::<Vec<_>>();
    steps.push(json!({"output":{"tools":[call,call]},"finish":"tool_calls"}));
    steps.push(json!({"output":{"text":ANSWER}}));
    json!({"version":1,"steps":steps})
}

pub(super) fn assert_interrupted_batch(
    fixture: &Fixture,
    outcome: &AgentOutcome,
    state: &summary::Summary,
) {
    assert_eq!(outcome.assistant_text(), [ANSWER]);
    assert_eq!(state.original_answers, 1);
    assert_eq!(state.tool_call_count, 8);
    assert!(state.ledger.as_array().expect("operation facts").len() < 20);
    invariants::no_duplicate_dispatch(fixture);
}

pub(super) fn yielded_script() -> Value {
    let mut script = script();
    let steps = script["steps"].as_array_mut().unwrap();
    let answer = steps.pop().unwrap();
    steps.push(json!({"output":{"cell":"placeholder"},"finish":"tool_calls"}));
    steps.extend((0..14).map(|index| json!({"output":{"text":format!("continue-with-live-cell-{index}")},"finish":"assistant_continuation"})));
    steps.push(json!({"output":{"tool":{"name":"wait","arguments":{"cell_id":"7","yield_time_ms":600_000}}},"finish":"tool_calls"}));
    steps.push(answer);
    script
}

pub(super) fn configure_yielded(fixture: &Fixture) {
    // Closed prefix debt stays below this threshold until the yielded cell's
    // children are below a later P; continuation rounds then force its sweep.
    fixture.set_ledger_retirement_high_water(40);
    let release = release_file(fixture);
    if release.exists() {
        std::fs::remove_file(&release).unwrap();
    }
    let mut script: Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    if script["steps"][6]["output"]["cell"].is_null() {
        return;
    }
    // Forks share the admitted workspace, but release gates belong to each
    // isolated cut. One replay must never release or reset another's process.
    let command = format!(
        "n=0; while test ! -f '{}'; do [ $((n += 1)) -le 10000 ] || exit 1; sleep 0.01; done; printf joined",
        release.display().to_string().replace('\'', "'\\''")
    );
    script["steps"][6]["output"]["cell"] = json!(format!(
        "await tools.exec_command({{cmd:'printf delivered'}}); await yield_control(); await tools.exec_command({{cmd:{},yield_time_ms:10000}}); text('closed');",
        serde_json::to_string(&command).unwrap()
    ));
    script["steps"][7]["stream"] = json!([{"gate":{"file":fixture.root.join("delivered-child")}}]);
    script["steps"][21]["gate"] = json!({"file":fixture.root.join("yielded-retired")});
    fixture.write_script(&script);
}

fn release_file(fixture: &Fixture) -> std::path::PathBuf {
    fixture.root.join("RETIREMENT_RELEASE")
}

#[test]
fn parallel_yielded_cuts_do_not_reset_each_others_release_gate() {
    let seed = Fixture::new(&yielded_script());
    let first = seed.fork(&yielded_script());
    let second = seed.fork(&yielded_script());
    assert_eq!(first.workspace(), second.workspace());
    configure_yielded(&first);
    std::fs::write(release_file(&first), b"").unwrap();
    configure_yielded(&second);
    assert!(release_file(&first).exists());
    assert!(!release_file(&second).exists());
}

#[test]
fn yielded_retirement_configuration_does_not_replace_the_followup() {
    let followup = get("ledger_retirement_yielded_cell").followup();
    let fixture = Fixture::new(&followup);
    configure_yielded(&fixture);
    let configured: Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    assert_eq!(configured, followup);
}

pub(super) fn yielded_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    let delivered = records
        .iter()
        .filter(|row| {
            row["kind"] == "tool" && !row["parent_call_id"].is_null() && row["state"] == "done"
        })
        .count();
    if delivered >= 7 {
        std::fs::write(fixture.root.join("delivered-child"), b"").unwrap();
    }
    if provider_dispatches(fixture) >= 22 {
        let frontier = summary::read_live_frontier(fixture);
        if yielded_group_survives_retirement(&frontier.ledger) {
            std::fs::write(release_file(fixture), b"").unwrap();
            std::fs::write(fixture.root.join("yielded-retired"), b"").unwrap();
        }
    }
    prompt_then_quit(fixture, records, state)
}

pub(super) fn provider_dispatches(fixture: &Fixture) -> usize {
    std::fs::read_to_string(fixture.root.join("evidence.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("provider_dispatched"))
        .count()
}

fn yielded_group_survives_retirement(
    ledger: &[plakous_offline_crash_harness::ledger_signature::LedgerFact],
) -> bool {
    let evaluations = ledger
        .iter()
        .filter(|row| row.kind == "composition_evaluation");
    if evaluations.clone().count() >= 7 {
        return false;
    }
    let Some(evaluation) = evaluations
        .filter(|row| row.state == "locator_bound")
        .map(|row| row.operation)
        .max()
    else {
        return false;
    };
    let provider = ledger
        .iter()
        .filter(|row| row.kind == "provider")
        .map(|row| row.operation)
        .max()
        .unwrap_or(0);
    ledger.iter().any(|row| {
        row.kind == "composition_child" && row.operation > evaluation && row.operation < provider
    })
}

pub(super) fn assert_yielded(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_eq!(
        outcome.assistant_text().last().map(String::as_str),
        Some(ANSWER)
    );
    assert_eq!(state.original_answers, 1);
    let ledger = state.ledger.as_array().expect("operation facts");
    assert!(fixture.root.join("yielded-retired").exists());
    assert!(ledger.len() < 48, "closed debt did not retire: {ledger:?}");
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|row| row["kind"] == "tool" && row["name"] == "exec" && row["state"] == "done")
            .map(|row| row["call_id"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        7
    );
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|row| row["kind"] == "tool" && row["name"] == "wait" && row["state"] == "done")
            .count(),
        1
    );
    invariants::no_duplicate_dispatch(fixture);
}

pub(super) fn assert_live(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_eq!(outcome.assistant_text(), [ANSWER]);
    assert_eq!(state.original_answers, 1);
    let ledger = state.ledger.as_array().expect("operation facts");
    assert!(
        ledger.len() < 30,
        "closed operation debt did not retire: {ledger:?}"
    );
    assert_eq!(
        ledger
            .iter()
            .filter(|row| row["kind"] == "provider")
            .count(),
        7
    );
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|row| row["kind"] == "tool" && row["state"] == "done" && row["name"] == "exec")
            .count(),
        6
    );
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|row| row["kind"] == "tool"
                && row["state"] == "done"
                && !row["parent_call_id"].is_null())
            .count(),
        6
    );
    invariants::no_duplicate_dispatch(fixture);
}
