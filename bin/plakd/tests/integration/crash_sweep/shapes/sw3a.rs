use super::super::summary;
use super::{ANSWER, AgentOutcome, Fixture, Value, assert_live};
use serde_json::json;

pub(super) fn promoted_background_job() -> Value {
    process_script(false)
}

pub(super) fn leftover_background_job() -> Value {
    let mut script = process_script(false);
    script["steps"][0]["output"]["tool"]["arguments"]["command"] =
        json!("sleep 600 >/dev/null 2>&1 & printf '%s' \"$!\" > SWEEP_JOB_PID; exit 0");
    script["steps"][0]["output"]["tool"]["arguments"]["yield_time_ms"] = json!(30000);
    script
}

pub(super) fn assert_leftover(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_live(fixture, outcome, state, 1, 2);
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|record| record["kind"] == "job_promoted")
            .count(),
        1
    );
    assert!(state.jobs.iter().all(|job| job["terminal"] == true));
    let pid = std::fs::read_to_string(fixture.root.join("sweep-job.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        assert_eq!(
            stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
            Some("Z"),
            "pilot quit left its leftover process alive"
        );
    }
    #[cfg(target_os = "macos")]
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err());
}

pub(super) fn sealed_family_artifacts() -> Value {
    process_script(true)
}

fn process_script(sealed: bool) -> Value {
    json!({"version": 1, "process_tools": true, "seal_family_artifacts": sealed,
    "steps": [
        {"output": {"tool": {"name": "process_session_start", "arguments": {
            "command": "printf '%s' \"$$\" > SWEEP_JOB_PID; read line",
            "workdir": ".", "login": false, "tty": false,
            "keep_stdin_open": true, "yield_time_ms": 0, "max_output_bytes": 8192
        }}}, "finish": "tool_calls"},
        {"output": {"text": ANSWER}, "finish": "stop"}
    ]})
}

pub(super) fn configure_process(fixture: &Fixture) {
    let mut script: Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    let pid = fixture.root.join("sweep-job.pid");
    let quoted = format!("'{}'", pid.display().to_string().replace('\'', "'\\''"));
    for step in script["steps"].as_array_mut().unwrap() {
        if let Some(command) = step["output"]["tool"]["arguments"]["command"].as_str() {
            step["output"]["tool"]["arguments"]["command"] =
                json!(command.replace("SWEEP_JOB_PID", &quoted));
        }
    }
    fixture.write_script(&script);
}

pub(super) fn assert_process(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_live(fixture, outcome, state, 1, 2);
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|record| record["kind"] == "job_promoted")
            .count(),
        1,
        "pilot did not promote exactly one live process"
    );
    assert!(
        fixture.root.join("sweep-job.pid").is_file(),
        "pilot process never ran"
    );
    let pid = std::fs::read_to_string(fixture.root.join("sweep-job.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
        "pilot quit left its promoted process alive"
    );
    assert!(
        state.jobs.iter().all(|job| job["terminal"] == true),
        "pilot quit left an unsettled durable job"
    );
}

pub(super) fn assert_sealed(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_process(fixture, outcome, state);
    let artifact = state
        .family_artifact
        .as_ref()
        .expect("pilot family artifact promotion");
    assert_eq!(artifact["family"], "responses");
    assert_eq!(
        artifact["fixture_native"], true,
        "pilot did not persist sealed native bytes"
    );
    assert!(artifact["schema"].as_u64().unwrap() > 0);
    assert!(artifact["codec"].as_u64().unwrap() > 0);
    assert_ne!(artifact["digest"].as_str().unwrap(), "");
}

#[test]
fn leftover_job_pilot_pins_group_promotion_and_settlement() {
    let (fixture, session) = super::super::driver::setup("leftover_background_job");
    super::super::driver::pilot(&fixture, &session, "leftover_background_job");
}

#[test]
fn promoted_job_pilot_pins_real_process_promotion_and_settlement() {
    let (fixture, session) = super::super::driver::setup("promoted_background_job");
    super::super::driver::pilot(&fixture, &session, "promoted_background_job");
}

#[test]
fn sealed_artifact_pilot_pins_real_native_promotion() {
    let (fixture, session) = super::super::driver::setup("sealed_family_artifacts");
    super::super::driver::pilot(&fixture, &session, "sealed_family_artifacts");
}
