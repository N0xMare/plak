//! A stale or broken evaluator must end the real product turn, not just its spawn future.

use super::*;

fn refused_worker(body: &str, cause: &str) {
    let fixture = Fixture::new(&code_mode_script(&json!([
        {"output": {"cell": "text('unreachable')"}, "finish": "tool_calls"},
        {"output": {"text": "unexpected continuation"}}
    ])));
    let worker = fixture.root.join("stale-worker");
    std::fs::write(&worker, format!("#!/bin/sh\n{body}\n")).expect("worker");
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).expect("executable");
    let mut command = fixture.command();
    command.arg("agent").env("PLAK_JS_HOST", &worker);
    let mut agent = AgentProcess::start_command(command, true);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    let started = Instant::now();
    agent.send(&prompt_frame("run a cell"));
    agent.wait_for("failed turn terminal", |record| {
        record["kind"] == "error"
            && record["message"]
                .as_str()
                .is_some_and(|text| text.contains("agent turn failed"))
    });
    assert!(started.elapsed() < Duration::from_secs(35));
    agent.wait_for("idle after failure", |record| {
        record["kind"] == "busy" && record["busy"] == false
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    outcome.assert_exactly_one_terminal_record();
    let output = outcome.lines.join("\n");
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|record| record["kind"] == "turn_lifecycle" && record["phase"] == "failed")
            .count(),
        1
    );
    assert!(outcome.records().iter().any(|record| {
        record["kind"] == "tool"
            && record["name"] == "exec"
            && record["state"]
                .as_str()
                .is_some_and(|state| state.starts_with("failed"))
    }));
    assert!(output.contains(cause), "{output}");
    if cause == "hello" {
        assert!(output.contains("did not answer within 30 s"), "{output}");
        assert!(
            output.contains("retry if the host is overloaded"),
            "{output}"
        );
    }
    assert!(
        output.contains("reinstall the matching plakous-js-host"),
        "{output}"
    );
    assert!(!output.contains("unexpected continuation"), "{output}");
}

#[test]
fn a_silent_worker_fails_the_turn_at_the_handshake_deadline() {
    refused_worker("exec sleep 120", "hello");
}

#[test]
fn an_old_protocol_worker_fails_the_turn() {
    refused_worker(
        "printf '%s\\n' '{\"kind\":\"ready\"}'; exec sleep 30",
        "frame bytes",
    );
}

#[test]
fn a_worker_exiting_before_hello_fails_the_turn() {
    refused_worker("exit 0", "closed before hello");
}

#[test]
fn a_worker_exiting_during_hello_fails_the_turn() {
    refused_worker("printf '\\000\\000'; exit 0", "truncated in its length");
}

#[test]
fn an_incompatible_hello_fails_the_turn() {
    let hello = json!({"frame": "hello", "revision": "plakous-cell-protocol.v0", "engine": "quickjs", "adapter": "old"}).to_string();
    let body = format!(
        "printf '\\000\\000\\000\\{:03o}%s' '{hello}'; exec sleep 30",
        hello.len()
    );
    refused_worker(&body, "plakous-cell-protocol.v0");
}
