//! A restart continues the handed-off root and its running child once on the
//! successor, with no client attached, and keeps a promoted job running.

use super::*;

fn child_script(fixture: &Fixture) -> PathBuf {
    fixture.root.join("child.json")
}

fn wait_child_dispatches(fixture: &Fixture, expected: usize, host: &Host) {
    let deadline = Instant::now() + WAIT;
    while !fixture.root.join("child-evidence.jsonl").exists()
        || crash::client_control::child_dispatches(fixture) < expected
    {
        assert!(
            Instant::now() < deadline,
            "the child did not reach dispatch {expected}: {:?}",
            host.lines.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_restart_to_a_replacement_continues_a_waiting_root_and_its_running_child_once() {
    let Some((fixture, mut source_script)) = root_wake::child_fixture(false) else {
        return;
    };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture);
    source_script["steps"][1]["stream"] = json!([
        {"text": PREFIX}, {"gate": {"file":fixture.root.join("root-not-released")}}
    ]);
    fixture.write_script(&source_script);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(
        3,
        attachment,
        "delegate and stay active across restart",
        PREFIX,
    );
    wait_child_dispatches(&fixture, 1, &host);
    drop(client);

    std::fs::write(
        child_script(&fixture),
        json!({"version": 1, "repeat_last_step": true, "steps": [{"stream": [
            {"text": "child continued"},
            {"gate": {"file": fixture.root.join("never-released")}}
        ]}]})
        .to_string(),
    )
    .unwrap();
    fixture.write_script(&json!({
        "version": 1, "tree": true, "durable_tree": true, "repeat_last_step": true,
        "product_child_script": child_script(&fixture),
        "steps": [{"output": {"text": ANSWER}}]
    }));
    let mut host = restart_to(&fixture, &mut host, &image, &[]);
    wait_child_dispatches(&fixture, 2, &host);
    let deadline = Instant::now() + WAIT;
    while root_record(&fixture, &session).1 == 0 {
        assert!(
            Instant::now() < deadline,
            "the root never continued: {:?}; {:?}",
            root_record(&fixture, &session),
            host.lines.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(crash::client_control::child_dispatches(&fixture), 2);
    assert_eq!(
        root_record(&fixture, &session).1,
        1,
        "exactly one root continuation"
    );
    stop(&mut host);
}

/// A promoted job of an idle root keeps running across a changed-triple
/// restart, and its terminal reaches the root once.
#[test]
fn a_promoted_root_job_survives_a_changed_triple_restart_and_reports_once() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let environment = candidate_environment(&image).to_vec();
    let root = shell_words::quote(fixture.root.to_str().unwrap()).into_owned();
    let command = format!(
        "base={root}/idle-bg; printf '%s %s\\n' \"$$\" \"$PPID\" >> \"$base.marker\"; \
         n=0; until [ -f \"$base-finish\" ]; do [ \"$n\" -lt 10000 ] || exit 42; n=$((n + 1)); sleep 0.02; done; printf 'retained\\n'"
    );
    fixture.write_script(&json!({"version": 1, "process_tools": true, "steps": [
        {"output": {"tool": {"name": "process_session_start", "arguments": {
            "command": command, "workdir": ".", "login": false, "tty": false,
            "keep_stdin_open": false, "yield_time_ms": 0, "max_output_bytes": 4096
        }}}, "finish": "tool_calls"},
        {"output": {"text": ANSWER}}
    ]}));
    let mut host = start_env(&fixture, &[], &environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "start a background job", ANSWER);
    let job = marked(&fixture, "idle", "bg");
    drop(client);

    fixture.write_script(
        &json!({"version": 1, "process_tools": true, "repeat_last_step": true,
        "steps": [{"output": {"text": ANSWER}}]}),
    );
    let mut host = restart_to(&fixture, &mut host, &image, &environment);
    runner::wait_local_runner_ready(&host);
    assert!(runner::alive(job.0), "the restart stopped the promoted job");
    let mut client = Client::connect(&host);
    client.attach(4, &node);
    std::fs::write(fixture.root.join("idle-bg-finish"), "").unwrap();
    wait_exit(job.0);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let deadline = Instant::now() + WAIT;
    loop {
        let (head, _) = identity(&fixture, &session);
        if !inspect_process_notices(&paths.runtime_database(), head)
            .unwrap()
            .is_empty()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the job terminal never reached the root"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    profiles::wait_idle(&mut client, &session);
    drop(client);
    stop(&mut host);
    let (head, _) = identity(&fixture, &session);
    assert_eq!(
        inspect_process_notices(&paths.runtime_database(), head).unwrap(),
        [1]
    );
    assert_eq!(marked(&fixture, "idle", "bg"), job, "the job was replayed");
}
