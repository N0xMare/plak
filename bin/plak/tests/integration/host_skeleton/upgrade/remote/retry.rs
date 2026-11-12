//! Retry seeding must share the serving connection, including its live effects.

use super::*;

fn wait_exit(host: &mut Host) {
    let deadline = Instant::now() + WAIT;
    while host.child.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn ready(fixture: &Fixture, source: &str) -> bool {
    receipts(fixture).pop().unwrap()["mappings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|mapping| mapping["source"] == source && mapping["ready"] == true)
}

fn wait_mapping(fixture: &Fixture, source: &str) {
    let deadline = Instant::now() + WAIT;
    while !ready(fixture, source) {
        assert!(Instant::now() < deadline, "successor never became Ready");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn foreground_script(fixture: &Fixture) -> serde_json::Value {
    let command = format!(
        "printf '%s %s\\n' \"$$\" \"$PPID\" > '{}'; n=0; while [ ! -f '{}' ]; do [ \"$n\" -lt 30000 ] || exit 1; n=$((n + 1)); sleep 0.01; done; printf survived > '{}'",
        fixture.root.join("local-job.marker").display(),
        fixture.root.join("retry-release").display(),
        fixture.root.join("retry-completed").display()
    );
    json!({"version":1,"process_tools":true,"steps":[
        {"output":{"tool":{"name":"process_session_start","arguments":{"command":command,"workdir":".","login":false,"tty":false,"keep_stdin_open":false,"yield_time_ms":300_000,"max_output_bytes":8192}}},"finish":"tool_calls"},
        {"output":{"text":ANSWER},"finish":"stop"}
    ]})
}

#[test]
fn host_upgrade_retry_keeps_an_existing_foreground_call_on_the_same_runner() {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    fixture.write_script(&foreground_script(&fixture));
    let gated = fixture.workspace().join("pending-workspace");
    std::fs::create_dir(&gated).unwrap();
    let first = seed(&fixture, &path, &id, 31);
    let second = seed_at(&fixture, &path, &id, 32, &gated);
    let mut source = start_env(
        &fixture,
        &[],
        &[
            ("PATH", path.clone()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "before-import".into()),
        ],
    );
    let _ = upgrade(&fixture, &path, &binary(&fixture));
    wait_exit(&mut source);
    let held = fixture.workspace().join("withheld-workspace");
    std::fs::rename(&gated, &held).unwrap();
    let mut destination = start_env(
        &fixture,
        &[],
        &[
            ("PATH", path.clone()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ],
    );
    wait_mapping(&fixture, &first);
    assert!(!ready(&fixture, &second));
    let session = successor(&fixture, &first);
    let mut client = Client::connect(&destination);
    let node = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
        .node;
    let attachment = client.attach(2, &node);
    assert_eq!(
        client.request(
            3,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("hold a foreground effect during retry")),
                blobs: Vec::new()
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let (pid, _) = runner::job(&fixture);
    assert!(runner::alive(pid));
    assert_eq!(
        runner::effect_settlements_in(&fixture, &session, DESTINATION, false),
        0,
        "the call must stay foreground rather than promote"
    );
    std::fs::rename(held, gated).unwrap();
    wait_mapping(&fixture, &second);
    assert!(
        runner::alive(pid),
        "retry import killed the existing runner service"
    );
    assert_eq!(
        runner::effect_settlements_in(&fixture, &session, DESTINATION, false),
        0
    );
    std::fs::write(fixture.root.join("retry-release"), "release").unwrap();
    loop {
        if let HostFrame::Event { observation, .. } = client.next()
            && format!("{observation:?}").contains(ANSWER)
        {
            break;
        }
    }
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("retry-completed")).unwrap(),
        "survived"
    );
    assert_eq!(
        runner::effect_settlements_in(&fixture, &session, DESTINATION, true),
        0
    );
    stop(&mut destination);
}

fn relocated_export(fixture: &Fixture, operation: &serde_json::Value, source: &str) -> PathBuf {
    let retained = fixture
        .home()
        .join("host/upgrades")
        .join(operation["id"].as_str().unwrap())
        .join(format!("{source}.json"));
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(retained).unwrap()).unwrap();
    let recovered = fixture.root.join("recovered-workspace");
    std::fs::create_dir(&recovered).unwrap();
    document["runner"] = json!({"name":"local","runner_id":"local"});
    document["workspace_root"] = json!(recovered);
    let copy = fixture.root.join("relocated-export.json");
    write_private(&copy, &document.to_string());
    copy
}

#[test]
fn host_upgrade_abandon_releases_the_operation_but_never_the_reserved_successor() {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    let sources = [
        seed(&fixture, &path, &id, 41),
        seed(&fixture, &path, &id, 42),
    ];
    let mut source = start_env(
        &fixture,
        &[],
        &[
            ("PATH", path.clone()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "before-import".into()),
        ],
    );
    let _ = upgrade(&fixture, &path, &binary(&fixture));
    wait_exit(&mut source);
    let config = fixture.home().join("config.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        original.replace(
            &format!("command = {:?}", env!("CARGO_BIN_EXE_plak")),
            "command = \"/unavailable-upgrade-runner\"",
        ),
    )
    .unwrap();
    let mut destination = start_env(
        &fixture,
        &[],
        &[
            ("PATH", path.clone()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ],
    );
    let pending = receipts(&fixture).pop().unwrap();
    let reserved = pending["mappings"][0]["successor"].as_str().unwrap();
    for target in [reserved, id.as_str(), reserved] {
        let result = output_env(
            &fixture,
            &["host", "upgrade", "--abandon", target],
            true,
            &[("PATH", &path)],
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let completed = receipts(&fixture).pop().unwrap();
    assert_eq!(completed["phase"], "acknowledged");
    assert!(
        completed["mappings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|mapping| mapping["abandoned"] == true && mapping["ready"] == false)
    );
    let mut client = Client::connect(&destination);
    assert_eq!(client.rows(1), []);
    assert!(!matches!(
        client.request(
            2,
            Op::Attach {
                node: NodeAddress {
                    tree_id: reserved.into(),
                    agent_id: reserved.into()
                }
            }
        ),
        Outcome::Ok(_)
    ));
    assert!(
        output(&fixture, &["session", "list"], true)
            .status
            .success()
    );
    assert!(upgrade(&fixture, &path, &binary(&fixture)).status.success());
    let copy = relocated_export(&fixture, &pending, &sources[0]);
    stop(&mut destination);
    let imported = output(
        &fixture,
        &["session", "import", copy.to_str().unwrap()],
        true,
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
}
