//! Settled placed roots import only in the successor host, after source custody ends.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

fn seed(fixture: &Fixture, path: &str, id: &str, ordinal: usize) -> String {
    let mut document: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/session-export-v12-default.json"
    ))
    .unwrap();
    let previous: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/session-export-v11.json")).unwrap();
    document["history"] = previous["history"].clone();
    document["source"]["session_id"] = json!(uuid::Uuid::new_v4().to_string());
    document["workspace_root"] = json!(fixture.workspace());
    document["runner"] = json!({"name":"remote", "runner_id":id});
    document["profile"] = json!({"root_template":"lead", "working_set":["lead"]});
    document["named_state"] = json!({"remote":ordinal});
    document["queue"] = json!([format!("remote paused {ordinal}")]);
    let file = fixture.root.join(format!("remote-seed-{ordinal}.json"));
    write_private(&file, &document.to_string());
    let imported = output_env(
        fixture,
        &["session", "import", file.to_str().unwrap()],
        false,
        &[("PATH", path)],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    String::from_utf8(imported.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plak --resume "))
        .unwrap()
        .to_owned()
}

fn upgrade(fixture: &Fixture, path: &str, binary: &str) -> std::process::Output {
    output_env(
        fixture,
        &["host", "upgrade", "--expect-binary", binary],
        true,
        &[("PATH", path)],
    )
}

fn export(fixture: &Fixture, path: &str, session: &str, name: &str) -> serde_json::Value {
    let file = fixture.root.join(name);
    let exported = output_env(
        fixture,
        &[
            "session",
            "export",
            session,
            "--out",
            file.to_str().unwrap(),
        ],
        true,
        &[("PATH", path)],
    );
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
}

#[test]
fn host_upgrade_remote_settled_roots_use_host_custody_and_recorded_successors() {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    fixture.write_script(&json!({"version":1, "steps":[{"output":{"text":ANSWER}}]}));
    let sources = [seed(&fixture, &path, &id, 1), seed(&fixture, &path, &id, 2)];
    let mut host = start_env(&fixture, &[], &[("PATH", path.clone())]);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &id);
    let row = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == sources[0])
        .unwrap();
    client.attach(2, &row.node);
    // A remote spelling must never acquire the host's project overrides.
    std::fs::create_dir_all(fixture.workspace().join(".plak")).unwrap();
    std::fs::write(fixture.workspace().join(".plak/config.toml"), "invalid [").unwrap();
    let binary = binary(&fixture);
    let completed = upgrade(&fixture, &path, &binary);
    assert!(
        completed.status.success(),
        "{}; destination: {:?}",
        String::from_utf8_lossy(&completed.stderr),
        host.lines.try_iter().collect::<Vec<_>>()
    );
    let socket = fixture.home().join(format!("host/run/{DESTINATION}.sock"));
    let mut client = Client::connect_at(&socket);
    runner::remote_ready(&mut client, &id);
    for (ordinal, source) in sources.iter().enumerate() {
        let next = successor(&fixture, source);
        let document = export(
            &fixture,
            &path,
            &next,
            &format!("remote-carried-{ordinal}.json"),
        );
        assert_eq!(document["runner"]["runner_id"], id);
        assert_eq!(document["named_state"], json!({"remote":ordinal + 1}));
        assert_eq!(
            document["queue"],
            json!([format!("remote paused {}", ordinal + 1)])
        );
        assert_eq!(
            client.request(
                11,
                Op::Attach {
                    node: NodeAddress {
                        tree_id: source.clone(),
                        agent_id: source.clone()
                    }
                }
            ),
            Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::Moved {
                node: NodeAddress {
                    tree_id: next.clone(),
                    agent_id: next
                }
            })
        );
    }
    assert!(upgrade(&fixture, &path, &binary).status.success());
    assert_eq!(receipts(&fixture).len(), 1);
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut host);
}

fn crash_cut(cut: &str) {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
    let sources = [
        seed(&fixture, &path, &id, 11),
        seed(&fixture, &path, &id, 12),
    ];
    let seed_cut = fixture.root.join("carry-cut");
    if cut == "runtime-created" {
        std::fs::write(&seed_cut, "runtime-returned").unwrap();
    }
    let mut host = start_env(
        &fixture,
        &[],
        &[
            ("PATH", path.clone()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", cut.into()),
        ],
    );
    let binary = binary(&fixture);
    let _ = upgrade(&fixture, &path, &binary);
    let deadline = Instant::now() + WAIT;
    while host.child.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "destination did not reach cut {cut}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let custody = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.home().join("runners/runtime-owner.lock"))
        .unwrap();
    custody
        .try_lock()
        .expect("source or killed destination retained shared custody");
    drop(custody);
    if seed_cut.exists() {
        std::fs::remove_file(seed_cut).unwrap();
    }
    let receipt = receipts(&fixture).pop().unwrap();
    assert_eq!(receipt["phase"], "committed");
    let mappings = receipt["mappings"].clone();
    let first = receipt["mappings"][0]["successor"].as_str().unwrap();
    let refused = output_env(
        &fixture,
        &["run", "--session", first, "--", "must not run"],
        true,
        &[("PATH", &path)],
    );
    assert!(!refused.status.success());
    let recovered = upgrade(&fixture, &path, &binary);
    assert!(
        recovered.status.success(),
        "{cut}: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(receipts(&fixture).pop().unwrap()["mappings"], mappings);
    for (ordinal, source) in sources.iter().enumerate() {
        let next = successor(&fixture, source);
        assert_eq!(
            export(
                &fixture,
                &path,
                &next,
                &format!("remote-recovered-{ordinal}.json")
            )["named_state"],
            json!({"remote":ordinal + 11})
        );
    }
    assert!(upgrade(&fixture, &path, &binary).status.success());
    assert_eq!(receipts(&fixture).len(), 1);
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
}

#[test]
fn host_upgrade_remote_recovers_surrender_before_destination_import() {
    crash_cut("before-import");
}

#[test]
fn host_upgrade_remote_recovers_partial_import_once() {
    crash_cut("after-import-root");
}

#[test]
fn host_upgrade_remote_recovers_canonical_create_before_product_binding() {
    crash_cut("runtime-created");
}

#[test]
fn host_upgrade_remote_recovers_after_import_before_ready() {
    crash_cut("before-ready");
}

#[test]
fn host_upgrade_remote_mismatch_refuses_before_drain_with_paired_upgrade_remedy() {
    let Some(fixture) = fixture() else { return };
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
    let source = seed(&fixture, &path, &id, 1);
    let mut host = start_env(&fixture, &[], &[("PATH", path.clone())]);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &id);
    let row = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == source)
        .unwrap();
    let attachment = client.attach(2, &row.node);
    let stub = fixture.root.join("different-snapshot");
    let hello = json!({"kind":"hello", "version":{"protocol":plakous::runner::protocol::PROTOCOL_VERSION,"source_revision":"different-revision","flavor":"different-flavor"}, "binary_id":null,"runner_id":id,"instance":"other","boot_id":"boot","machine_id":"machine","descriptor":{},"capabilities":[],"path":null,"home":null,"job_bound":8});
    std::fs::write(&stub, format!("#!/usr/bin/env python3\nimport sys\ndata={hello:?}.encode()\nsys.stdout.buffer.write(len(data).to_bytes(4,'big')+data)\nsys.stdout.buffer.flush()\nsys.stdin.buffer.read()\n", hello=hello.to_string())).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = fixture.home().join("config.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        original.replace(
            &format!("command = {:?}", env!("CARGO_BIN_EXE_plak")),
            &format!("command = {stub:?}"),
        ),
    )
    .unwrap();
    let binary = binary(&fixture);
    let refused = upgrade(&fixture, &path, &binary);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("matching snapshot"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("VersionMismatch"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    std::fs::write(config, original).unwrap();
    assert_eq!(
        client.request(3, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
}

#[test]
fn host_upgrade_remote_capacity_and_pairing_changes_refuse_before_drain() {
    for case in ["small", "unknown", "pairing", "profile"] {
        let Some(fixture) = fixture() else { return };
        let (_daemon, path, id) = runner::remote_fixture(&fixture);
        fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
        let source = seed(&fixture, &path, &id, 1);
        let mut environment = vec![("PATH", path.clone())];
        if case == "small" || case == "unknown" {
            environment.push((
                "PLAK_TEST_HOST_UPGRADE_INPUT_LIMIT",
                if case == "small" { "0" } else { "unknown" }.into(),
            ));
        }
        let mut host = start_env(&fixture, &[], &environment);
        let mut client = Client::connect(&host);
        runner::remote_ready(&mut client, &id);
        let row = client
            .rows(1)
            .into_iter()
            .find(|row| row.session == source)
            .unwrap();
        let attachment = client.attach(2, &row.node);
        let pin = fixture.home().join("runners/remote/runner.json");
        let original = std::fs::read(&pin).unwrap();
        let template = fixture.home().join("agents/lead.toml");
        let saved_template = std::fs::read(&template).unwrap();
        if case == "pairing" {
            let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
            changed["machine_id"] = json!("a-different-machine");
            std::fs::write(&pin, serde_json::to_vec(&changed).unwrap()).unwrap();
        } else if case == "profile" {
            std::fs::remove_file(&template).unwrap();
        }
        let refused = upgrade(&fixture, &path, &binary(&fixture));
        assert!(!refused.status.success(), "{case}");
        std::fs::write(&pin, original).unwrap();
        std::fs::write(&template, saved_template).unwrap();
        assert_eq!(
            client.request(3, Op::Detach { attachment }),
            Outcome::Ok(Answer::Detached),
            "{case}"
        );
        assert!(!fixture.home().join("host/upgrades").exists(), "{case}");
        stop(&mut host);
    }
}
