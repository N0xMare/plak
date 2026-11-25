//! Same-key host upgrade and offline migration over real binaries in isolated homes.

use std::time::{Duration, Instant};

use super::*;

mod bootstrap;

const DESTINATION: &str = "r999.e999.p999.t999";

mod drain;
mod lifecycle;
mod paired;
pub(super) mod preservation;
mod reopen;
mod retained_jobs;
mod revisions;
mod runtime_epoch;
mod verdicts;

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

pub(super) fn seed(fixture: &Fixture, ordinal: usize) -> String {
    seed_in(fixture, ordinal, &fixture.workspace())
}

fn seed_in(fixture: &Fixture, ordinal: usize, workspace: &std::path::Path) -> String {
    let mut document: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/session-export-v13.json")).unwrap();
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
        .find_map(|line| line.strip_prefix("resume with: plakd run --session "))
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

fn require_upgrade(fixture: &Fixture, host: &Host, stage: &str, result: &std::process::Output) {
    if result.status.success() {
        return;
    }
    fixture.preserve();
    let home = fixture.home().join("host");
    let lock = home.join("host.lock");
    let custody = std::fs::File::open(&lock).map(|file| {
        let held = file.try_lock();
        let holder = std::fs::read_to_string(&lock);
        (held, holder)
    });
    let run_entries = std::fs::read_dir(home.join("run")).map(|entries| {
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>()
    });
    let child = host.child.as_ref().map(|child| child.id());
    let startup: Vec<_> = host.lines.try_iter().collect();
    panic!(
        "upgrade {stage}: {}; fixture={}; home custody={custody:?}; run entries={run_entries:?}; \
         fixture host pid={child:?}; host stderr={startup:?}",
        String::from_utf8_lossy(&result.stderr),
        fixture.root.display(),
    );
}

/// The held guard in this disposable home proves the process belongs to this fixture.
pub(super) struct HomeHost(pub(super) PathBuf);

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
fn changed_key_refuses_before_drain_and_migrates_only_after_stop() {
    let Some(fixture) = fixture() else { return };
    let session = seed(&fixture, 81);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let id = binary(&fixture);
    let result = output(&fixture, &["host", "upgrade", "--expect-binary", &id], true);
    assert!(!result.status.success());
    let detail = String::from_utf8_lossy(&result.stderr);
    assert!(
        detail.contains("host_upgrade_offline_migration_required"),
        "{detail}"
    );
    assert!(detail.contains("stop the source host"), "{detail}");
    assert!(client.rows(1).iter().any(|row| row.session == session));
    assert!(!fixture.home().join("state").join(DESTINATION).exists());

    drop(client);
    stop(&mut host);
    let before = export(&fixture, &session, false, "offline-source.json");
    let path = fixture.root.join("offline-source.json");
    let command = ["session", "import", path.to_str().unwrap()];
    let imported = output(&fixture, &command, true);
    assert!(imported.status.success(), "{imported:?}");
    let stdout = String::from_utf8(imported.stdout).unwrap();
    let next = stdout
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plakd run --session "))
        .unwrap();
    assert_ne!(next, session);
    let duplicate = output(&fixture, &command, true);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains(next));

    let after = export(&fixture, next, true, "offline-destination.json");
    for field in ["history", "queue", "named_state"] {
        assert_eq!(before[field], after[field], "{field}");
    }
    let mut destination = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_STATE_KEY", DESTINATION.into())],
    );
    let mut client = Client::connect(&destination);
    let row = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == next)
        .unwrap();
    client.attach(2, &row.node);
    stop(&mut destination);
}

#[test]
fn v3_host_coordinator_refuses_stale_source_and_removed_carry_before_drain() {
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
    for (field, value, reason) in [
        (
            "source_instance",
            json!("stale-instance"),
            "host_upgrade_stale_source",
        ),
        (
            "source_key",
            json!(DESTINATION),
            "host_upgrade_stale_source",
        ),
        (
            "destination_key",
            json!(DESTINATION),
            "host_upgrade_offline_migration_required",
        ),
        (
            "wait_seconds",
            json!(1),
            "host_upgrade_offline_migration_required",
        ),
        (
            "abandon",
            json!("desk"),
            "host_upgrade_offline_migration_required",
        ),
    ] {
        let mut stream = UnixStream::connect(fixture.home().join("host/upgrade.sock")).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let identity: serde_json::Value = serde_json::from_str(&line).unwrap();
        let mut request = json!({
            "source_instance":identity["instance"], "source_key":identity["key"],
            "destination":env!("CARGO_BIN_EXE_plakd"), "destination_key":identity["key"],
            "expected_binary":binary, "wait_seconds":0,
            "drain":{"mode":"bounded","milliseconds":60000}
        });
        assert_eq!(identity["protocol"], 3);
        request[field] = value;
        writeln!(stream, "{request}").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let answer: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(answer["status"], "refused");
        assert!(answer["detail"].as_str().unwrap().contains(reason));
        if reason == "host_upgrade_offline_migration_required" {
            assert!(
                answer.get("reason").is_none(),
                "legacy v3 must parse the refusal"
            );
        }
    }
    assert_eq!(
        client.request(3, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
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
    let bindings = image_command(&fixture, &image)
        .args(["session", "check-binding", "--all"])
        .output()
        .unwrap();
    let bindings: serde_json::Value = serde_json::from_slice(&bindings.stdout).unwrap();
    for row in bindings["sessions"].as_array().unwrap() {
        assert!(row["binding_change"].is_null(), "{row}");
        if let Some(unavailable) = row["unavailable"].as_str() {
            // The scripted executor is not a release-owned production tool.
            assert!(unavailable.contains("offline."), "{row}");
        }
    }
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

fn seed_binding_heads(fixture: &Fixture) {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/hosting/hosted/tests/fixtures/current-bindings");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(source.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["runtime_schema"],
        plakous::sessions::RUNTIME_STORE_SCHEMA_VERSION
    );
    assert_eq!(
        manifest["runtime_epoch"],
        plakous::sessions::RUNTIME_FORMAT_CUT
    );
    assert_eq!(manifest["cases"].as_array().unwrap().len(), 17);
    let bytes = std::fs::read(source.join("runtime.sqlite3")).unwrap();
    assert_eq!(plakous::model::sha256_hex(&[&bytes]), manifest["sha256"]);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    std::fs::create_dir_all(paths.runtime_database().parent().unwrap()).unwrap();
    std::fs::write(paths.runtime_database(), bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            paths.runtime_database(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
}

#[test]
fn host_upgrade_binding_regression_refuses_before_surrender_and_prints_live_rows() {
    let Some(fixture) = fixture() else { return };
    // Binding compatibility is independent of a native-format cut. Produce the
    // source in this format instead of treating an old native database as current.
    seed_binding_heads(&fixture);
    let image = replacement_image(&fixture);
    let cut = image.to_str().unwrap().to_owned();
    let mut host = start_env(&fixture, &[], &[("PLAK_TEST_BINDING_CUT_IMAGE", cut)]);
    let mut client = Client::connect(&host);
    let baseline = output(&fixture, &["session", "check-binding", "--all"], false);
    assert!(baseline.status.success(), "{baseline:?}");
    let baseline: serde_json::Value = serde_json::from_slice(&baseline.stdout).unwrap();
    assert_eq!(baseline["compatible"], true);
    assert_eq!(baseline["sessions"].as_array().unwrap().len(), 17);
    let runtime_session = baseline["sessions"][0]["session"].as_str().unwrap();
    let probe = image_command(&fixture, &image)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    let probe: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let refused = image_command(&fixture, &image)
        .args([
            "host",
            "upgrade",
            "--expect-binary",
            probe["binary_id"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let report = String::from_utf8_lossy(&refused.stderr);
    assert!(report.contains("binding_preflight_refused"), "{report}");
    assert!(!report.contains("phase=exact_reopen"), "{report}");
    assert!(
        report.contains("offending rows") && report.contains(runtime_session),
        "{report}"
    );
    assert_eq!(client.request(1, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
}

#[test]
fn web_service_upgrade_probe_refuses_an_invalid_home_template() {
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    fixture.write_config(
        "[web]\nallow = [\"a@example.com\"]\norigin = \"https://plak.example.ts.net\"\n",
    );
    let service = fixture.root.join("service.json");
    let listen = format!("unix:{}", fixture.root.join("web.sock").display());
    write_private(
        &service,
        &json!({"arguments":["host","--web",listen],"placements":[]}).to_string(),
    );
    let probe = || {
        fixture
            .base_command()
            .args(["version", "--host-upgrade-probe", "--host-upgrade-service"])
            .arg(&service)
            .output()
            .unwrap()
    };
    let ready = probe();
    assert!(
        ready.status.success(),
        "{}",
        String::from_utf8_lossy(&ready.stderr)
    );
    write_private(&fixture.home().join("agents/broken.toml"), "account = [");
    let refused = probe();
    let reason = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{reason}");
    assert!(reason.contains("broken.toml"), "{reason}");
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
    let image = replacement_image(&fixture);
    let probe = image_command(&fixture, &image)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success(), "{probe:?}");
    let facts: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let id = facts["binary_id"].as_str().unwrap();
    let upgrade = || {
        image_command(&fixture, &image)
            .args(["host", "upgrade", "--expect-binary", id])
            .output()
            .unwrap()
    };
    std::fs::write(ui.join("csp.json"), "invalid").unwrap();
    let refused = upgrade();
    assert!(!refused.status.success());
    assert_eq!(
        client.request(3, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    std::fs::write(ui.join("csp.json"), csp).unwrap();
    let completed = upgrade();
    assert!(
        completed.status.success(),
        "{}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(
        export(&fixture, &source, false, "web-before-stop.json")["named_state"],
        json!({"answer":8})
    );
    stop(&mut host);
}

fn identity(fixture: &Fixture, key: &str, id: &str) -> (plakous::sessions::SessionLocator, String) {
    let state = fixture.home().join("state").join(key);
    let product = rusqlite::Connection::open_with_flags(
        state.join("product.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let locator: String = product
        .query_row("SELECT locator FROM sessions WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    let locator: plakous::sessions::SessionLocator = locator.parse().unwrap();
    let native = rusqlite::Connection::open_with_flags(
        state.join("runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let line = native
        .query_row(
            "SELECT line_id FROM branches WHERE session_id=?1 AND branch_id=?2",
            rusqlite::params![
                locator.session_id().to_string(),
                locator.branch_id().to_string()
            ],
            |row| row.get(0),
        )
        .unwrap();
    (locator, line)
}
