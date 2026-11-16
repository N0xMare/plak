use super::*;

fn seed_script() -> Value {
    json!({"version":1, "image_input":true, "content_sensitive_projection":true,
        "maximum_projected_input":1_000_000, "steps":[{"output":{"text":"residency seed"}}], "repeat_last_step":true})
}

pub(super) fn admission_script() -> Value {
    seed_script()
}
pub(super) fn tool_script() -> Value {
    let mut script = seed_script();
    script["tool_result_images"] = json!(8);
    script
}
pub(super) fn continue_script() -> Value {
    let mut script = seed_script();
    script["tool_result_images"] = json!(0);
    script
}

pub(super) fn configure_admission(fixture: &Fixture) {
    configure(fixture, "admission");
}
pub(super) fn configure_tool(fixture: &Fixture) {
    configure(fixture, "tool");
}
pub(super) fn configure_continue(fixture: &Fixture) {
    configure(fixture, "continue");
}

fn configure(fixture: &Fixture, fence: &str) {
    let mut png = vec![0; 3_024];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    png[16..20].copy_from_slice(&1_u32.to_be_bytes());
    png[20..24].copy_from_slice(&1_u32.to_be_bytes());
    let path = fixture.workspace().join("residency.png");
    if !path.is_file() {
        std::fs::write(path, png).unwrap();
    }
    let config_path = fixture.home().join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        config_path,
        config.replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    if !summary::paths(fixture).runtime_database().is_file() {
        return;
    }
    let mut script: Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    if script["steps"] != seed_script()["steps"] {
        return;
    }
    script["repeat_last_step"] = json!(false);
    script["steps"] = match fence {
        "tool" => json!([
            {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
            {"output":{"text":ANSWER}}
        ]),
        "continue" => json!([
            {"output":{"images":8},"finish":"assistant_continuation"},
            {"output":{"text":ANSWER}}
        ]),
        _ => json!([{"output":{"text":ANSWER}}]),
    };
    fixture.write_script(&script);
}

pub(super) fn setup(agent: &mut AgentProcess) {
    for (index, count) in [7, 7, 6].into_iter().enumerate() {
        agent.send(&json!({"v":1,"kind":"prompt","text":format!("residency seed {index}"),"images":vec!["residency.png"; count]}));
        agent.wait_for_completed_turns(index + 1);
    }
}

pub(super) fn admission_lifecycle(
    fixture: &Fixture,
    records: &[Value],
    state: &mut Lifecycle,
) -> Option<Value> {
    if !state.sent && records.iter().any(|row| row["kind"] == "ready") {
        state.sent = true;
        return Some(
            json!({"v":1,"kind":"prompt","text":"crash-sweep-original-prompt","images":vec!["residency.png";8]}),
        );
    }
    prompt_then_quit(fixture, records, state)
}

pub(super) fn assert_admission(
    fixture: &Fixture,
    outcome: &AgentOutcome,
    state: &summary::Summary,
) {
    assert_residency(fixture, outcome, state, 0, 1);
}
pub(super) fn assert_tool(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_residency(fixture, outcome, state, 1, 2);
}
pub(super) fn assert_continue(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_residency(fixture, outcome, state, 0, 2);
}

fn assert_residency(
    fixture: &Fixture,
    outcome: &AgentOutcome,
    state: &summary::Summary,
    tools: i64,
    providers: usize,
) {
    assert_live(fixture, outcome, state, tools, providers);
    let provenance = plakous_offline_crash_harness::ledger_signature::inspect_compaction_strategy(
        &summary::paths(fixture).runtime_database(),
        state.head.parse().unwrap(),
    )
    .unwrap()
    .expect("residency provenance");
    assert_eq!(provenance.0, "ResidencyPlaceholdersV1");
    assert_eq!(
        state
            .ledger
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "compaction" && row["state"] == "finalized")
            .count(),
        1
    );
    assert_compact_row(outcome);
}

#[test]
fn residency_fence_pilots_commit_without_a_paid_compaction() {
    for shape in [
        "image_residency_admission",
        "image_residency_tool_batch",
        "image_residency_continue",
    ] {
        let (seed, session) = super::super::driver::setup(shape);
        super::super::driver::pilot(&seed, &session, shape);
    }
}

#[test]
fn image_residency_crash_cuts_recover_without_redispatch() {
    super::super::run(
        &[
            "image_residency_admission",
            "image_residency_tool_batch",
            "image_residency_continue",
        ],
        4,
        1,
        true,
        "residency",
    );
}
