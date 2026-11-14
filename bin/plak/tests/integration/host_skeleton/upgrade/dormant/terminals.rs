//! An exited original worker is evidence, not an already-consumed agent notice.
use super::*;

#[test]
fn host_move_dormant_retains_an_unconsumed_terminal_job_and_its_output() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let configuration = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&configuration).unwrap();
    std::fs::write(
        configuration,
        text.replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    let destination = binary(&fixture);
    let release = fixture.root.join("job-release");
    let done = fixture.root.join("job-done");
    let child = fixture.root.join("child.json");
    std::fs::write(&child, "{\"version\":1,\"steps\":[]}").unwrap();
    fixture.write_script(&json!({"version":1,"tree":true,"durable_tree":true,
        "tree_process_tools":true,"product_child_script":child,"steps":[
        {"output":{"tool":{"name":"process_session_start","arguments":{
            "command":format!("n=0; while ! test -f '{}'; do n=$((n + 1)); [ \"$n\" -le 1200 ] || exit 71; sleep 0.05; done; printf retained-terminal-output; printf done > '{}'", release.display(), done.display()),
            "workdir":".","login":false,"tty":false,"keep_stdin_open":false,
            "yield_time_ms":0,"max_output_bytes":4096
        }}},"finish":"tool_calls"},{"output":{"text":ANSWER}}
    ]}));
    let mut host = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
    runner::wait_local_runner_ready(&host);
    let (session, node) = finished::create_and_prompt(&fixture, &mut host, "start a retained job");
    finished::wait_idle(&fixture, &session, 1);
    suppress_wake(&fixture, &mut host, &session, &node);
    std::fs::write(&release, "finish").unwrap();
    let deadline = Instant::now() + WAIT;
    while !done.exists() {
        assert!(
            Instant::now() < deadline,
            "retained process did not terminate"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &destination],
        true,
        &[(ENABLED, "1")],
    );
    assert!(result.status.success(), "terminal job refused: {result:?}");
    assert_eq!(successor(&fixture, &session), session);
    let authority = receipts(&fixture).pop().unwrap();
    let group = authority["groups"][0]["id"].as_str().unwrap();
    let capsule: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .home()
                .join("host/upgrades")
                .join(authority["id"].as_str().unwrap())
                .join(format!("{group}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    let jobs = &capsule["sessions"][0]["heads"][0]["jobs"];
    assert_eq!(jobs["next_number"], 2);
    assert_eq!(
        jobs["rows"].as_array().unwrap().len(),
        1,
        "terminal proof must not consume the native notice"
    );
    assert_eq!(
        jobs["rows"][0]["terminal"]["tail"],
        "retained-terminal-output"
    );
    stop(&mut host);
}

fn suppress_wake(fixture: &Fixture, host: &mut Host, session: &str, node: &NodeAddress) {
    // Ordinary idle Esc holds automatic notices, without stopping the promoted process.
    let mut client = Client::connect(host);
    let attachment = client.attach(1, node);
    assert_eq!(
        client.request(
            2,
            Op::Intent {
                attachment,
                intent: SessionIntent::Interrupt,
                client_id: None,
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let deadline = Instant::now() + WAIT;
    while !plak::store::sessions::load_session(&paths, session)
        .unwrap()
        .child_wake_suppressed
    {
        assert!(
            Instant::now() < deadline,
            "idle interrupt did not hold automatic notices"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        client.request(3, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
}
