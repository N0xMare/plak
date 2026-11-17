//! The child-runner shapes (spec 01 §8.4): a session on a real runner daemon
//! promotes a job there. Every cut kills the host at one of its commits.
//! `runner_job_survives_host_loss` leaves the daemon running, so a job the
//! cut found running is still running when the successor host reopens;
//! `runner_restart_unknown` kills the daemon at the cut too, so the reopened
//! host finds a new runner instance that holds none of the old one's jobs.

use super::super::super::offline_support::ChildRunner;
use super::super::summary;
use super::{ANSWER, AgentOutcome, Fixture, Value, assert_live};
use serde_json::json;

/// The file each start of the job appends its own pid and its parent's to.
const JOB_MARKER: &str = "runner-job.marker";

pub(super) fn job_script() -> Value {
    json!({"version": 1, "process_tools": true,
    "steps": [
        {"output": {"tool": {"name": "process_session_start", "arguments": {
            "command": "printf '%s %s\\n' \"$$\" \"$PPID\" >> RUNNER_JOB_MARKER; read line",
            "workdir": ".", "login": false, "tty": false,
            "keep_stdin_open": true, "yield_time_ms": 0, "max_output_bytes": 8192
        }}}, "finish": "tool_calls"},
        {"output": {"text": ANSWER}, "finish": "stop"}
    ]})
}

/// Selects the fixture's child runner, and starts its daemon unless it runs.
pub(super) fn configure(fixture: &Fixture) {
    let runner = ChildRunner::of(fixture);
    let mut script: Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    let marker = fixture.root.join(JOB_MARKER);
    let quoted = format!("'{}'", marker.display().to_string().replace('\'', "'\\''"));
    for step in script["steps"].as_array_mut().unwrap() {
        if let Some(command) = step["output"]["tool"]["arguments"]["command"].as_str() {
            step["output"]["tool"]["arguments"]["command"] =
                json!(command.replace("RUNNER_JOB_MARKER", &quoted));
        }
    }
    fixture.write_script(&script);
    if runner.pid().is_none() {
        runner.start();
    }
}

/// The job's pid and its parent's, once it has run.
fn job(fixture: &Fixture) -> Option<(i32, i32)> {
    let text = std::fs::read_to_string(fixture.root.join(JOB_MARKER)).ok()?;
    let (pid, parent) = text.lines().next()?.split_once(' ')?;
    Some((pid.parse().ok()?, parent.parse().ok()?))
}

/// How many times the job started: a call that ran twice shows here.
fn starts(fixture: &Fixture) -> usize {
    std::fs::read_to_string(fixture.root.join(JOB_MARKER)).map_or(0, |text| text.lines().count())
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// The pilot promoted one job and quit without stopping its daemon-owned work.
pub(super) fn assert_job(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_live(fixture, outcome, state, 1, 2);
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|record| record["kind"] == "job_promoted")
            .count(),
        1,
        "pilot did not promote exactly one runner job"
    );
    let jobs = outcome
        .records()
        .into_iter()
        .filter(|record| {
            record["kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("job"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        jobs.iter()
            .filter(|record| record["kind"] == "job_terminal")
            .count(),
        0,
        "pilot job records: {jobs:?}"
    );
    let (pid, parent) = job(fixture).expect("the runner job never ran");
    assert_eq!(
        starts(fixture),
        1,
        "the pilot started its job more than once"
    );
    assert_eq!(
        Some(parent),
        ChildRunner::at(fixture).pid(),
        "the job's parent is not the runner daemon"
    );
    assert!(alive(pid), "pilot quit stopped its daemon-owned job");
    assert_eq!(state.jobs.len(), 1, "pilot lost its durable job");
    assert!(
        state.jobs.iter().all(|job| job["terminal"] == false),
        "pilot quit invented a job terminal"
    );
}

/// What the cut leaves the successor host. The daemon outlives the host;
/// so does a job it started, even after the answer and ordinary release.
/// The restart shape then kills
/// the daemon too, and the successor's bridge meets a new runner instance.
pub(in super::super) fn after_kill(shape: &str, fixture: &Fixture) {
    let runner = ChildRunner::at(fixture);
    if shape.starts_with("runner_") {
        assert!(
            starts(fixture) <= 1,
            "the killed host started its job twice"
        );
    }
    match shape {
        "runner_job_survives_host_loss" => {
            assert!(
                runner.pid().is_some(),
                "the runner daemon died with the host"
            );
            if let Some((pid, _)) = job(fixture) {
                assert!(alive(pid), "the runner job died with the host");
            }
        }
        // The runner restarts before anything of the successor runs, so
        // the replacement is the harness's own and ends with the fixture.
        "runner_restart_unknown" => {
            runner.kill();
            runner.start();
        }
        _ => {}
    }
}

/// What the successor left: the job started at most once over the cut and
/// both reopens, never again for a tool call the cut had already run.
pub(in super::super) fn after_recovery(shape: &str, fixture: &Fixture, before: &summary::Summary) {
    if shape.starts_with("runner_") {
        assert!(starts(fixture) <= 1, "the successor started the job again");
    }
    if shape == "runner_job_survives_host_loss"
        && before.jobs.iter().any(|job| job["terminal"] == false)
    {
        let (pid, _) = job(fixture).expect("retained job lost its start marker");
        assert!(alive(pid), "the successor stopped the retained runner job");
        retained_jobs(before, &summary::read_converged(fixture));
    }
}

fn retained_jobs(before: &summary::Summary, after: &summary::Summary) {
    for job in before.jobs.iter().filter(|job| job["terminal"] == false) {
        let retained = after
            .jobs
            .iter()
            .find(|row| row["number"] == job["number"])
            .expect("the successor lost the retained job row");
        assert_eq!(retained["terminal"], false, "retained job became terminal");
        assert_eq!(
            retained["owner_generation"], job["owner_generation"],
            "retained job changed owner generation"
        );
    }
}

#[test]
fn retained_job_oracle_rejects_lost_terminal_and_replaced_rows() {
    let row = json!({"number": 1, "owner_generation": 7, "terminal": false});
    let before = summary::Summary {
        jobs: vec![row.clone()],
        ..summary::Summary::default()
    };
    retained_jobs(&before, &before);
    for jobs in [
        vec![],
        vec![json!({"number": 2, "owner_generation": 7, "terminal": false})],
        vec![json!({"number": 1, "owner_generation": 8, "terminal": false})],
        vec![json!({"number": 1, "owner_generation": 7, "terminal": true})],
    ] {
        let after = summary::Summary {
            jobs,
            ..summary::Summary::default()
        };
        assert!(std::panic::catch_unwind(|| retained_jobs(&before, &after)).is_err());
    }
}

#[test]
fn runner_job_pilot_promotes_and_retains_a_job_in_the_daemon() {
    let (fixture, session) = super::super::driver::setup("runner_job_survives_host_loss");
    super::super::driver::pilot(&fixture, &session, "runner_job_survives_host_loss");
}
