//! Host-mode counterparts of the runner-loss and startup recovery shapes.

use super::super::{invariants, trace};
use super::*;

pub(in super::super) fn is_host(shape: &str) -> bool {
    matches!(
        shape,
        "runner_job_survives_host_loss_host"
            | "runner_restart_unknown_host"
            | "host_handoff_keeps_jobs"
            | "host_crash_mid_readopt"
            | "host_crash_mid_stop"
            | "embedded_open_after_host_crash"
    )
}

pub(super) fn script() -> Value {
    runner::job_script()
}

pub(super) fn configure(fixture: &Fixture) {
    let mut script: Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    let quoted = format!(
        "'{}'",
        fixture
            .root
            .join("host-runner-job.marker")
            .display()
            .to_string()
            .replace('\'', "'\\''")
    );
    for step in script["steps"].as_array_mut().unwrap() {
        if let Some(command) = step["output"]["tool"]["arguments"]["command"].as_str() {
            let command = command.replace("RUNNER_JOB_MARKER", &quoted).replace(
                "read line",
                &format!(
                    "n=0; while [ ! -e {} ]; do [ \"$n\" -lt 300 ] || exit 1; n=$((n + 1)); sleep 1; done",
                    fixture.root.join("host-job-release").display()
                ),
            );
            step["output"]["tool"]["arguments"]["command"] = json!(command);
        }
    }
    fixture.write_script(&script);
}

pub(super) fn configure_handoff(fixture: &Fixture) {
    configure(fixture);
    let mut script: Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    script["steps"][1] = json!({"stream": [{"text": ANSWER}, {"stall": 30000}]});
    fixture.write_script(&script);
}

fn starts(fixture: &Fixture) -> usize {
    std::fs::read_to_string(fixture.root.join("host-runner-job.marker"))
        .map_or(0, |text| text.lines().count())
}

fn job(fixture: &Fixture) -> Option<i32> {
    std::fs::read_to_string(fixture.root.join("host-runner-job.marker"))
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn runner_lines(fixture: &Fixture) -> Vec<plakous::runner::protocol::LineStatus> {
    use plakous::runner::protocol::{Bind, PROTOCOL_VERSION, VersionTriple};
    use plakous::runner::{ClientWindows, RunnerClient};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let socket = fixture
            .home()
            .join(format!("host/runner/v{PROTOCOL_VERSION}/run/runner.sock"));
        let Ok(stream) = tokio::net::UnixStream::connect(socket).await else {
            return Vec::new();
        };
        let bind = Bind {
            host_id: std::fs::read_to_string(fixture.home().join("host-id"))
                .unwrap()
                .trim()
                .to_owned(),
            host_instance: "sweep-inspector".into(),
            version: VersionTriple::current("inspect", "test"),
            inspect: true,
        };
        let (client, _) =
            RunnerClient::connect_home_local(stream, bind, ClientWindows::default(), None)
                .await
                .unwrap();
        let lines = client.lines().await.unwrap();
        client.disconnect();
        lines
    })
}

pub(super) fn assert_running(fixture: &Fixture, _: &AgentOutcome, _: &summary::Summary) {
    assert_eq!(starts(fixture), 1);
    assert!(
        alive(job(fixture).unwrap()),
        "a host handoff stopped its daemon job"
    );
}

pub(super) fn assert_stopped(fixture: &Fixture, _: &AgentOutcome, _: &summary::Summary) {
    assert_eq!(starts(fixture), 1);
    assert!(
        !alive(job(fixture).unwrap()),
        "the recorded stop was not finished"
    );
}

pub(in super::super) fn after_kill(
    fixture: &Fixture,
    session: &str,
    shape: &str,
    commits: &[trace::Commit],
) -> Value {
    assert!(starts(fixture) <= 1);
    let pid = job(fixture);
    let lines = runner_lines(fixture);
    let held = !lines.is_empty();
    let was_promoted = lines.into_iter().any(|line| {
        plakous::tools::runner::decode_jobs_snapshot(line.jobs)
            .unwrap()
            .1
            .iter()
            .any(|job| job.status == plakous::tools::JobStatus::Running)
    });
    if shape != "host_crash_mid_stop"
        && let Some(pid) = pid
    {
        assert!(alive(pid), "a cut stopped the daemon job");
    }
    if shape == "runner_restart_unknown_host" {
        super::super::super::offline_support::kill_local_daemon_only(&fixture.home());
    }
    let before = summary::read(fixture);
    if shape == "embedded_open_after_host_crash" && held {
        assert_embedded_refused(fixture, session);
        assert_eq!(
            summary::read(fixture).jobs,
            before.jobs,
            "the embedded refusal changed job outcomes"
        );
    }
    let dispatches = invariants::dispatches(fixture).len();
    let mut followup = script();
    followup["steps"] = json!([{"output": {"text": FOLLOWUP}, "finish": "stop"}]);
    fixture.write_script(&followup);
    let unknown_jobs = if shape == "runner_restart_unknown_host" {
        before
            .jobs
            .iter()
            .filter(|job| job["terminal"] == false)
            .map(|job| u32::try_from(job["number"].as_u64().unwrap()).unwrap())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let first = crate::host_skeleton::sweep::recover(fixture, session, FOLLOWUP, &unknown_jobs);
    let after = summary::read_converged(fixture);
    assert!(starts(fixture) <= 1, "recovery started the job again");
    if shape == "host_crash_mid_stop" {
        assert!(
            !alive(pid.unwrap()),
            "recovery did not finish the recorded stop"
        );
    } else if let Some(pid) = pid
        && shape != "runner_restart_unknown_host"
    {
        assert_eq!(
            alive(pid),
            was_promoted,
            "transfer must stop only foreground processes"
        );
    }
    if shape == "runner_restart_unknown_host" {
        for job in before.jobs.iter().filter(|job| job["terminal"] == false) {
            let id = format!("p{}", job["number"].as_u64().unwrap());
            assert!(
                first
                    .records()
                    .iter()
                    .any(|row| row["kind"] == "job_terminal"
                        && row["job_id"] == id
                        && row["status"] == "outcome_unknown"),
                "a lost job was not OutcomeUnknown"
            );
        }
    }
    let once = invariants::dispatches(fixture).len();
    assert!(
        once <= dispatches + 1,
        "recovery dispatched more than one continuation"
    );
    crate::host_skeleton::sweep::recover(fixture, session, FOLLOWUP, &[]);
    assert_eq!(
        invariants::dispatches(fixture).len(),
        once,
        "the next restart continued twice"
    );
    invariants::no_duplicate_dispatch(fixture);
    json!({"outcome": if after.original_answers > 0 { "COMPLETED_ONCE" } else { "HELD" }, "invariants": true,
        "dispatches": once, "before": before, "recovered": after, "global_order": commits,
        "global_ordinal": commits.last().unwrap().ordinal})
}

fn assert_embedded_refused(fixture: &Fixture, session: &str) {
    let outcome = fixture
        .command()
        .args(["agent", "--session", session])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&outcome.stdout),
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(outcome.status.code(), Some(7), "{text}");
    assert!(
        text.contains("this session's processes run in the local runner"),
        "{text}"
    );
}
