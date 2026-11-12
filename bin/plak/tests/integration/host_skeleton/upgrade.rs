//! Settled cross-key carry uses real binaries and isolated homes, never prompt replay.

use std::time::{Duration, Instant};

use super::*;

const DESTINATION: &str = "r999.e999.p999.t999";

mod recovery;
mod remote;
mod selection;
mod topology;

fn output(fixture: &Fixture, arguments: &[&str], destination: bool) -> std::process::Output {
    output_env(fixture, arguments, destination, &[])
}

fn output_env(
    fixture: &Fixture,
    arguments: &[&str],
    destination: bool,
    environment: &[(&str, &str)],
) -> std::process::Output {
    let mut command = fixture.command();
    command.args(arguments).envs(environment.iter().copied());
    if destination {
        command.env("PLAK_TEST_STATE_KEY", DESTINATION);
    }
    command.output().unwrap()
}

fn binary(fixture: &Fixture) -> String {
    let result = output(fixture, &["version", "--host-upgrade-probe"], true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()["binary_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn seed(fixture: &Fixture, ordinal: usize) -> String {
    seed_in(fixture, ordinal, &fixture.workspace())
}

fn seed_in(fixture: &Fixture, ordinal: usize, workspace: &std::path::Path) -> String {
    let mut document: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/session-export-v11.json")).unwrap();
    document["source"]["session_id"] = json!(uuid::Uuid::new_v4().to_string());
    document["workspace_root"] = json!(workspace);
    document["named_state"] = json!({"answer": ordinal});
    document["queue"] = json!([format!("paused input {ordinal}")]);
    let path = fixture.root.join(format!("seed-{ordinal}.json"));
    write_private(&path, &document.to_string());
    let result = output(
        fixture,
        &["session", "import", path.to_str().unwrap()],
        false,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plak --resume "))
        .unwrap()
        .to_owned()
}

fn receipts(fixture: &Fixture) -> Vec<serde_json::Value> {
    let directory = fixture.home().join("host/upgrades");
    let mut records: Vec<serde_json::Value> = std::fs::read_dir(directory)
        .unwrap()
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path().join("receipt.json")).ok())
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        .collect();
    records.sort_by_key(|receipt| receipt["sequence"].as_u64().unwrap());
    records
}

fn successor(fixture: &Fixture, source: &str) -> String {
    receipts(fixture)
        .iter()
        .rev()
        .filter(|receipt| {
            matches!(
                receipt["phase"].as_str(),
                Some("committed" | "acknowledged")
            )
        })
        .flat_map(|receipt| receipt["mappings"].as_array().unwrap())
        .find(|mapping| mapping["source"] == source)
        .unwrap()["successor"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn export(fixture: &Fixture, session: &str, destination: bool, name: &str) -> serde_json::Value {
    let path = fixture.root.join(name);
    let result = output(
        fixture,
        &[
            "session",
            "export",
            session,
            "--out",
            path.to_str().unwrap(),
        ],
        destination,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn upgrade(fixture: &Fixture, binary: &str) -> std::process::Output {
    output(
        fixture,
        &["host", "upgrade", "--expect-binary", binary],
        true,
    )
}

/// Recovery can create a different serving PID. The held guard in this
/// disposable home proves that the process belongs to this fixture.
struct HomeHost(PathBuf);

impl Drop for HomeHost {
    fn drop(&mut self) {
        let path = self.0.join("host/host.lock");
        let Ok(file) = std::fs::File::open(&path) else {
            return;
        };
        if !matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
            return;
        }
        let Ok(holder) = std::fs::read_to_string(path) else {
            return;
        };
        let Some(pid) = holder
            .split_whitespace()
            .next()
            .and_then(|pid| pid.parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw)
        else {
            return;
        };
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        let deadline = Instant::now() + WAIT;
        while matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn host_upgrade_settled_roots_round_trip_once_and_reconnect() {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let roots = [seed(&fixture, 1), seed(&fixture, 2)];
    let mut host = start(&fixture);
    let id = binary(&fixture);
    let result = upgrade(&fixture, &id);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(receipts(&fixture).len(), 1);
    let repeated = upgrade(&fixture, &id);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(result.stdout, repeated.stdout);
    assert_eq!(receipts(&fixture).len(), 1);
    let target_socket = fixture.home().join(format!("host/run/{DESTINATION}.sock"));
    let mut client = Client::connect_at(&target_socket);
    for (index, source) in roots.iter().enumerate() {
        let next = successor(&fixture, source);
        let document = export(&fixture, &next, true, &format!("carried-{index}.json"));
        assert_eq!(document["version"], 12);
        assert_eq!(document["named_state"], json!({"answer":index + 1}));
        assert_eq!(
            document["queue"],
            json!([format!("paused input {}", index + 1)])
        );
        assert_eq!(document["source_subagents"], 0);
        assert_eq!(
            client.request(
                40 + index as u64,
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
        let refused = output(
            &fixture,
            &["run", "--session", source, "--", "must not run"],
            false,
        );
        assert!(!refused.status.success());
        let refusal = String::from_utf8_lossy(&refused.stderr);
        assert!(
            refusal.contains("source_fenced") || refusal.contains("session_archived"),
            "{refusal}"
        );
    }
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut host);
}

fn host_upgrade_receipt_crash_cut(cut: &str) {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let source = seed(&fixture, 42);
    let mut host = start_env(&fixture, &[], &[("PLAK_TEST_HOST_UPGRADE_CUT", cut.into())]);
    let id = binary(&fixture);
    let _ = upgrade(&fixture, &id);
    let deadline = Instant::now() + WAIT;
    while host.child.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "cut did not end the host: {cut}");
        std::thread::sleep(Duration::from_millis(10));
    }
    let committed = matches!(
        receipts(&fixture).last().unwrap()["phase"].as_str(),
        Some("committed" | "acknowledged")
    );
    let recovered = upgrade(&fixture, &id);
    if committed {
        assert!(
            recovered.status.success(),
            "{cut}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
    } else {
        assert!(
            !recovered.status.success(),
            "precommit recovery must restore the source: {cut}"
        );
        assert!(String::from_utf8_lossy(&recovered.stderr).contains("host_upgrade_aborted"));
        let _ = export(&fixture, &source, false, "source-after-cut.json");
        let completed = upgrade(&fixture, &id);
        assert!(
            completed.status.success(),
            "{cut}: {}",
            String::from_utf8_lossy(&completed.stderr)
        );
    }

    let next = successor(&fixture, &source);
    let document = export(&fixture, &next, true, "target-after-cut.json");
    assert_eq!(document["named_state"], json!({"answer":42}), "{cut}");
    assert_eq!(
        receipts(&fixture)
            .iter()
            .filter(|receipt| receipt["phase"] == "acknowledged")
            .count(),
        1,
        "{cut}"
    );
    assert!(
        upgrade(&fixture, &id).status.success(),
        "lost-ack retry: {cut}"
    );
    assert_eq!(successor(&fixture, &source), next);
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
}

#[test]
fn host_upgrade_recovers_after_receipt() {
    host_upgrade_receipt_crash_cut("after-receipt");
}

#[test]
fn host_upgrade_recovers_after_export() {
    host_upgrade_receipt_crash_cut("after-export");
}

#[test]
fn host_upgrade_recovers_after_mapping() {
    host_upgrade_receipt_crash_cut("after-mapping");
}

#[test]
fn host_upgrade_recovers_after_import() {
    host_upgrade_receipt_crash_cut("after-import");
}

#[test]
fn host_upgrade_recovers_after_commit() {
    host_upgrade_receipt_crash_cut("after-commit");
}

#[test]
fn host_upgrade_recovers_before_answer() {
    host_upgrade_receipt_crash_cut("before-answer");
}

#[test]
fn host_upgrade_recovers_after_ack() {
    host_upgrade_receipt_crash_cut("after-ack");
}
#[test]
fn host_upgrade_wait_timeout_restores_admissions_without_surrender() {
    let Some(fixture) = fixture() else { return };
    let release = fixture.root.join("provider-release");
    let mut host = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_PROVIDER_RELEASE", release.display().to_string())],
    );
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    assert_eq!(
        client.request(
            3,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("remain busy")),
                blobs: Vec::new(),
            },
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    wait_for_provider_dispatch(&fixture);
    let id = binary(&fixture);
    for wait in ["0", "1"] {
        let refused = output(
            &fixture,
            &["host", "upgrade", "--expect-binary", &id, "--wait", wait],
            true,
        );
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("running_turn"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert_eq!(client.request(10, Op::Ping), Outcome::Ok(Answer::Pong));
    }
    assert!(!fixture.home().join("host/upgrades").exists());
    assert!(
        !fixture
            .home()
            .join(format!("state/{DESTINATION}/runtime.sqlite3"))
            .exists()
    );
    std::fs::write(release, "continue").unwrap();
    stop(&mut host);
}

#[test]
fn host_upgrade_stale_source_instance_or_key_refuses_without_draining() {
    let Some(fixture) = fixture() else { return };
    let source = seed(&fixture, 9);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let row = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == source)
        .unwrap();
    let attachment = client.attach(2, &row.node);
    let binary = binary(&fixture);
    for field in ["source_instance", "source_key"] {
        let mut stream = UnixStream::connect(fixture.home().join("host/upgrade.sock")).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let identity: serde_json::Value = serde_json::from_str(&line).unwrap();
        let mut request = json!({
            "source_instance":identity["instance"], "source_key":identity["key"],
            "destination":env!("CARGO_BIN_EXE_plak"), "destination_key":DESTINATION,
            "expected_binary":binary, "wait_seconds":0
        });
        request[field] = json!(if field == "source_key" {
            DESTINATION
        } else {
            "stale-instance"
        });
        writeln!(stream, "{request}").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let answer: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(answer["status"], "refused");
        assert!(
            answer["detail"]
                .as_str()
                .unwrap()
                .contains("host_upgrade_stale_source")
        );
    }
    assert_eq!(
        client.request(3, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
}

#[test]
fn host_upgrade_cross_key_refuses_daemon_jobs_without_disturbing_custody() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, node) = runner::start_job(&fixture, false);
    let (pid, daemon) = runner::job(&fixture);
    let mut client = Client::connect(&host);
    profiles::wait_idle(&mut client, &session);
    let attachment = client.attach(11, &node);
    let id = binary(&fixture);
    let refused = upgrade(&fixture, &id);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("daemon_jobs"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        client.request(12, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    assert!(runner::alive(pid) && runner::alive(daemon));
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
}

#[test]
fn host_upgrade_archived_root_with_daemon_jobs_is_not_carried_or_checked() {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let (mut host, session, _) = runner::start_job(&fixture, false);
    let mut client = Client::connect(&host);
    profiles::wait_idle(&mut client, &session);
    // Even legacy unsettled archives are outside the carried root selection.
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let product = rusqlite::Connection::open(paths.product_database()).unwrap();
    product.busy_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        product
            .execute("UPDATE sessions SET archived=1 WHERE id=?1", [&session])
            .unwrap(),
        1
    );
    drop(product);
    let upgraded = upgrade(&fixture, &binary(&fixture));
    assert!(
        upgraded.status.success(),
        "{}",
        String::from_utf8_lossy(&upgraded.stderr)
    );
    assert_eq!(receipts(&fixture).pop().unwrap()["mappings"], json!([]));
    stop(&mut host);
}

#[test]
fn host_upgrade_preflight_refuses_capacity_versions_config_and_wrong_identity_before_drain() {
    for case in ["small", "unknown", "versions", "config", "binary"] {
        let Some(fixture) = fixture() else { return };
        let source = seed(&fixture, 7);
        let environment = match case {
            "small" => vec![("PLAK_TEST_HOST_UPGRADE_INPUT_LIMIT", "0".into())],
            "unknown" => vec![("PLAK_TEST_HOST_UPGRADE_INPUT_LIMIT", "unknown".into())],
            "versions" => vec![("PLAK_TEST_HOST_UPGRADE_IMPORT_REFUSAL", "1".into())],
            _ => Vec::new(),
        };
        let mut host = start_env(&fixture, &[], &environment);
        let mut client = Client::connect(&host);
        let row = client
            .rows(1)
            .into_iter()
            .find(|row| row.session == source)
            .unwrap();
        let attachment = client.attach(2, &row.node);
        let id = binary(&fixture);
        let config = fixture.home().join("config.toml");
        let saved = std::fs::read(&config).unwrap();
        if case == "config" {
            std::fs::write(&config, "not valid [").unwrap();
        }
        let result = upgrade(
            &fixture,
            if case == "binary" {
                "000000000000"
            } else {
                &id
            },
        );
        assert!(!result.status.success(), "{case}");
        std::fs::write(&config, saved).unwrap();
        assert_eq!(
            client.request(3, Op::Detach { attachment }),
            Outcome::Ok(Answer::Detached),
            "preflight drained the actor: {case}"
        );
        assert!(!fixture.home().join("host/upgrades").exists(), "{case}");
        assert!(
            !fixture
                .home()
                .join(format!("state/{DESTINATION}/runtime.sqlite3"))
                .exists(),
            "{case}"
        );
        stop(&mut host);
    }
}

fn image_command(fixture: &Fixture, image: &std::path::Path) -> std::process::Command {
    let template = fixture.command();
    let mut command = std::process::Command::new(image);
    command.env_clear().envs(
        template
            .get_envs()
            .filter_map(|(name, value)| value.map(|value| (name, value))),
    );
    command.current_dir(template.get_current_dir().unwrap());
    command
}

#[test]
fn host_upgrade_same_key_uses_planned_handoff_and_preserves_daemon_jobs() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, _) = runner::start_job(&fixture, false);
    let (pid, daemon) = runner::job(&fixture);
    let mut client = Client::connect(&host);
    profiles::wait_idle(&mut client, &session);
    let dispatches = crash::dispatches(&fixture);
    let image = replacement_image(&fixture);
    let probe = image_command(&fixture, &image)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success());
    let probe: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let id = probe["binary_id"].as_str().unwrap();
    assert_ne!(id, binary(&fixture));
    let replaced = image_command(&fixture, &image)
        .args(["host", "upgrade", "--expect-binary", id])
        .output()
        .unwrap();
    assert!(
        replaced.status.success(),
        "{}",
        String::from_utf8_lossy(&replaced.stderr)
    );
    runner::wait_local_runner_ready(&host);
    assert!(
        Client::connect(&host)
            .rows(1)
            .iter()
            .any(|row| row.session == session)
    );
    assert!(runner::alive(pid) && runner::alive(daemon));
    assert!(!fixture.home().join("host/upgrades").exists());
    assert_eq!(crash::dispatches(&fixture), dispatches);
    stop(&mut host);
}

fn replacement_image(fixture: &Fixture) -> PathBuf {
    let image = fixture.root.join("replacement");
    std::fs::copy(env!("CARGO_BIN_EXE_plak"), &image).unwrap();
    #[cfg(not(target_os = "macos"))]
    std::fs::OpenOptions::new()
        .append(true)
        .open(&image)
        .unwrap()
        .write_all(b"\0")
        .unwrap();
    #[cfg(target_os = "macos")]
    assert!(
        std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-", "--identifier", "upgrade-fixture"])
            .arg(&image)
            .status()
            .unwrap()
            .success()
    );
    image
}

#[test]
fn host_upgrade_preflights_web_assets_and_preserves_resolved_service_arguments() {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let source = seed(&fixture, 8);
    let ui = fixture.root.join("ui");
    std::fs::create_dir_all(&ui).unwrap();
    let csp = r#"{"header":"default-src 'self'; frame-ancestors 'none'"}"#;
    std::fs::write(ui.join("csp.json"), csp).unwrap();
    std::fs::write(ui.join("index.html"), "<!doctype html>upgrade").unwrap();
    let mut host = start_with(
        &fixture,
        &[
            "--web",
            "127.0.0.1:0",
            "--dev-login",
            "a@example.com",
            "--ui",
            "../ui",
        ],
    );
    let mut client = Client::connect(&host);
    let row = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == source)
        .unwrap();
    let attachment = client.attach(2, &row.node);
    let id = binary(&fixture);
    std::fs::write(ui.join("csp.json"), "invalid").unwrap();
    let refused = upgrade(&fixture, &id);
    assert!(!refused.status.success());
    assert_eq!(
        client.request(3, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    std::fs::write(ui.join("csp.json"), csp).unwrap();
    let completed = upgrade(&fixture, &id);
    assert!(
        completed.status.success(),
        "{}",
        String::from_utf8_lossy(&completed.stderr)
    );
    let next = successor(&fixture, &source);
    assert_eq!(
        export(&fixture, &next, true, "web-carried.json")["named_state"],
        json!({"answer":8})
    );
    stop(&mut host);
}
