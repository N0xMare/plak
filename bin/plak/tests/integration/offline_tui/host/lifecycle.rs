//! Leaving a socket face preserves a busy root and its running children.

use super::*;

fn review_child_fixture() -> Fixture {
    let fixture = Fixture::with_mode(&json!({"version": 1, "steps": []}), Mode::Host);
    let child_script = fixture.root.join("child-script.json");
    write_json(
        &child_script,
        &gated_child(&fixture.root.join("child-gate-never")),
    );
    fixture.write_script(&product_child_root(
        &child_script,
        &json!([
            {"output": {"tool": {"name": "spawn_agent", "arguments": {
                "template": "review",
                "tier": "read_only",
                "description": "reviewer",
                "prompt": "review the diff",
                "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
            }}}, "finish": "tool_calls"},
            {"output": {"text": "review child is running"}}
        ]),
    ));
    enable_subagents(&fixture);
    fixture
}

fn review_end(terminal: Terminal, exit: &str) -> String {
    if exit == "ctrl-q" {
        let mut terminal = terminal;
        terminal.send(b"\x11");
        return format!("{:?}", terminal.finish_with_screen());
    }
    assert!(
        Command::new("kill")
            .arg(format!("-{exit}"))
            .arg(terminal.pid().to_string())
            .status()
            .unwrap()
            .success()
    );
    format!("{:?}", terminal.finish_with_screen())
}

/// Every way a host-mode terminal ends with work running (Ctrl+Q, `SIGTERM`,
/// `SIGHUP`, i.e. terminal loss) detaches: a busy root stays busy in the
/// host, a running child is never redispatched, and a fresh attach finds the
/// same work without a replay.
fn review_busy_root_survives(exit: &str) {
    let fixture = Fixture::with_mode(
        &json!({"version": 1, "repeat_last_step": true, "steps": [{"stream": [{"text": "review busy root"}, {"stall": 30000}]}]}),
        Mode::Host,
    );
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"keep this turn running\r");
    terminal.wait_screen("review busy root");
    terminal.wait_screen("Esc stop");
    let paid = provider_dispatches(&fixture);
    let ended = review_end(terminal, exit);
    assert!(!ended.contains("Quit plak?"), "{exit}: {ended}");
    let mut directory = Directory::connect(&fixture);
    let rows = directory.rows();
    assert_eq!(rows.len(), 1, "{exit}: {rows:?}");
    assert_eq!(
        rows[0]["activity"], "busy",
        "{exit} stopped or ended the root turn: {rows:?}"
    );
    assert_eq!(provider_dispatches(&fixture), paid, "{exit}");
    let session = rows[0]["session"].as_str().unwrap().to_owned();
    let mut again = Terminal::spawn(&fixture, Launch::Resume(&session));
    // A streaming delta is not durable transcript. Reattach must show the
    // admitted prompt and busy work, not replay that face's old text delta.
    again.wait_screen("keep this turn running");
    again.wait_screen("Esc stop");
    assert_eq!(
        provider_dispatches(&fixture),
        paid,
        "{exit}: re-attach redispatched"
    );
    again.send(b"/stop\r");
    again.wait_where("the stop", |frame| {
        frame.contains("session work stopped")
            || frame.contains("stopped: the session's work ended")
            || frame.contains("⊘ interrupted")
    });
    again.quit();
    eprintln!("review exit {exit}: busy root kept; rows {rows:?}");
}

fn review_running_child_survives(exit: &str) {
    let fixture = review_child_fixture();
    let mut terminal = Terminal::spawn_sized(&fixture, Launch::Fresh, 40, 220);
    terminal.wait_ready();
    terminal.send(b"start a reviewer\r");
    terminal.wait_screen("review child is running");
    terminal.wait_absent("Esc stop");
    wait_for_child_dispatches(&fixture, 1);
    let paid = (provider_dispatches(&fixture), child_dispatches(&fixture));
    let ended = review_end(terminal, exit);
    assert!(!ended.contains("Quit plak?"), "{exit}: {ended}");
    let mut directory = Directory::connect(&fixture);
    let rows = directory.rows();
    assert_ne!(rows[0]["activity"], "stopped", "{exit}: {rows:?}");
    let session = rows[0]["session"].as_str().unwrap().to_owned();
    let mut again = Terminal::spawn_sized(&fixture, Launch::Resume(&session), 40, 220);
    let screen = again.wait_screen("review child is running");
    assert!(
        !screen.contains("a lost host interrupted")
            && !screen.contains("recovered an interrupted turn"),
        "{exit}: {screen}"
    );
    assert_eq!(
        (provider_dispatches(&fixture), child_dispatches(&fixture)),
        paid,
        "{exit}: the child or root was redispatched"
    );
    again.quit();
    eprintln!("review exit {exit}: child kept; rows {rows:?}");
}

#[test]
fn review_ctrl_q_detaches_a_busy_root() {
    review_busy_root_survives("ctrl-q");
}

#[test]
fn review_sigterm_detaches_a_busy_root() {
    review_busy_root_survives("TERM");
}

#[test]
fn review_sighup_detaches_a_busy_root() {
    review_busy_root_survives("HUP");
}

#[test]
fn review_ctrl_q_detaches_a_running_child() {
    review_running_child_survives("ctrl-q");
}

#[test]
fn review_sigterm_detaches_a_running_child() {
    review_running_child_survives("TERM");
}

#[test]
fn review_sighup_detaches_a_running_child() {
    review_running_child_survives("HUP");
}
