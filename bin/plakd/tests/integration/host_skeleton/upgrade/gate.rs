use super::*;
use std::{path::Path, process::Command};

fn backup_fixture(fixture: &Fixture, id: &str) -> std::process::Output {
    output_env(
        fixture,
        &["host", "upgrade", "--expect-binary", id, "--backup-store"],
        false,
        &[
            ("PLAK_TEST_FORCE_HOST_UPGRADE", "1"),
            ("PLAK_TEST_HOST_UPGRADE_BACKUP_BYTES", "1"),
        ],
    )
}

#[test]
fn an_over_bound_backup_never_requests_surrender() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let id = binary(&fixture);
    let result = backup_fixture(&fixture, &id);
    let report = String::from_utf8_lossy(&result.stderr);
    assert!(
        !result.status.success() && report.contains("host_upgrade_backup_refused"),
        "{report}"
    );
    assert_eq!(client.request(1, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(!fixture.home().join("host/upgrade-checkpoint").exists());
    stop(&mut host);
}

fn successor(fixture: &Fixture, image: &Path) -> Command {
    let facts = image_command(fixture, image)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(facts.status.success());
    let probe: serde_json::Value = serde_json::from_slice(&facts.stdout).unwrap();
    let mut command = image_command(fixture, image);
    command.args([
        "host",
        "upgrade",
        "--expect-binary",
        probe["binary_id"].as_str().unwrap(),
        "--drain",
        "0",
    ]);
    command
}

fn during_exec(
    fixture: &Fixture,
    image: &Path,
    env: &[(&str, String)],
    change: impl FnOnce(),
) -> (Host, std::process::Output) {
    let gate = fixture.root.join("exec-ready");
    std::fs::write(&gate, []).unwrap();
    let mut environment = env.to_vec();
    environment.push((
        "PLAK_TEST_HOST_BEFORE_PAIRED_EXEC",
        gate.display().to_string(),
    ));
    let host = start_env(fixture, &[], &environment);
    let mut command = successor(fixture, image);
    let child = std::thread::spawn(move || command.output().unwrap());
    loop {
        let line = host
            .lines
            .recv_timeout(WAIT)
            .expect("source never entered paired exec");
        if line.contains("waiting at paired exec gate") {
            break;
        }
    }
    change();
    std::fs::remove_file(gate).unwrap();
    (host, child.join().unwrap())
}

#[test]
fn a_post_surrender_integrity_failure_blocks_service_manager_restarts() {
    let Some(fixture) = fixture() else { return };
    let image = replacement_image(&fixture);
    let (mut host, result) = during_exec(&fixture, &image, &[], || {
        let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
        let db = rusqlite::Connection::open(paths.product_database()).unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=OFF;
            INSERT INTO queue_attachments VALUES('missing','0','orphan','image/png',x'00');",
        )
        .unwrap();
    });
    assert_health_refused(&result);
    // A manager configured to restart failures must not launch the old
    // service link. The requester still reported the typed failure.
    assert!(host.child.take().unwrap().wait().unwrap().success());
    let mut verdict = image_command(&fixture, &image);
    verdict.args(["host", "upgrade-verdict"]);
    let inspected = verdict.output().unwrap();
    assert!(inspected.status.success(), "{inspected:?}");
    let report: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(report["state"], "failed");
    assert_eq!(report["phase"], "health");
    assert_eq!(report["version"], 1);
    let ack = |id: &str| {
        image_command(&fixture, &image)
            .args(["host", "upgrade-verdict", "--acknowledge", id])
            .output()
            .unwrap()
    };
    let id = report["id"].as_str().unwrap();
    let refused = ack(id);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("did not pass"));
    // Forcing an ordinary start on the successor is also fenced, even
    // after the cause has been repaired but not acknowledged.
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let db = rusqlite::Connection::open(paths.product_database()).unwrap();
    db.execute("DELETE FROM queue_attachments WHERE item_id='missing'", [])
        .unwrap();
    for _ in 0..2 {
        let restart = image_command(&fixture, &image)
            .arg("host")
            .output()
            .unwrap();
        let report = String::from_utf8_lossy(&restart.stderr);
        assert_eq!(restart.status.code(), Some(5), "{report}");
        assert!(
            report.contains("inherited upgrade did not reach Ready"),
            "{report}"
        );
        assert!(!fixture.home().join("host/upgrade.sock").exists());
    }
    let stale = ack(&uuid::Uuid::new_v4().to_string());
    assert!(!stale.status.success(), "{stale:?}");
    assert!(String::from_utf8_lossy(&stale.stderr).contains("host_upgrade_verdict_changed"));
    let cleared = ack(id);
    assert!(cleared.status.success(), "{cleared:?}");
    let cleared: serde_json::Value = serde_json::from_slice(&cleared.stdout).unwrap();
    assert_eq!(cleared["acknowledged"], true);
    let mut restarted =
        await_serving_host(spawn_host(image_command(&fixture, &image).arg("host"))).unwrap();
    assert_eq!(
        Client::connect(&restarted).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );
    stop(&mut restarted);
}

fn assert_health_refused(result: &std::process::Output) {
    let detail = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(5), "{detail}");
    assert!(
        detail.contains(
            "host_upgrade_fix_forward_required: destination health failed after surrender"
        ),
        "{detail}"
    );
}

#[test]
fn a_binding_regression_after_preflight_withholds_readiness() {
    let Some(fixture) = fixture() else { return };
    seed_binding_heads(&fixture);
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let cut = fixture.root.join("binding-cut");
    let (mut host, result) = during_exec(
        &fixture,
        &image,
        &[
            ("PLAK_TEST_BINDING_CUT_IMAGE", image.display().to_string()),
            ("PLAK_TEST_BINDING_CUT_AFTER", cut.display().to_string()),
        ],
        || std::fs::write(cut, []).unwrap(),
    );
    assert_health_refused(&result);
    assert!(host.child.take().unwrap().wait().unwrap().success());
    let log = host.lines.try_iter().collect::<Vec<_>>().join("\n");
    assert!(
        log.contains("destination binding compatibility did not pass"),
        "{log}"
    );
}

#[test]
fn a_preexisting_binding_change_does_not_fail_destination_health() {
    let Some(fixture) = fixture() else { return };
    seed_binding_heads(&fixture);
    let _cleanup = HomeHost(fixture.home());
    let image = fixture.command().get_program().to_owned();
    let path = Path::new(&image).canonicalize().unwrap();
    let cut = path.display().to_string();
    let mut host = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_BINDING_CUT_IMAGE", cut.clone())],
    );
    let report = output_env(
        &fixture,
        &["session", "check-binding", "--all"],
        false,
        &[("PLAK_TEST_BINDING_CUT_IMAGE", &cut)],
    );
    assert!(!report.status.success());
    assert!(String::from_utf8_lossy(&report.stdout).contains("fixture.revised-executor"));
    let result = successor(&fixture, &path)
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
        .output()
        .unwrap();
    require_upgrade(&fixture, &host, "pre-existing binding change", &result);
    assert_eq!(
        Client::connect(&host).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );
    stop(&mut host);
}

#[test]
fn a_withdrawn_image_restores_the_fenced_source() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture);
    let (mut host, result) = during_exec(&fixture, &image, &[], || {
        std::fs::remove_file(&image).unwrap();
    });
    let detail = String::from_utf8_lossy(&result.stderr);
    assert!(
        !result.status.success() && detail.contains("host_upgrade_source_restored"),
        "{detail}"
    );
    assert_eq!(
        Client::connect(&host).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );
    stop(&mut host);
}

#[test]
fn a_lost_surrender_reply_is_outcome_unknown() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture);
    let mut host = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_HOST_UPGRADE_CUT", "before-answer".into())],
    );
    let result = successor(&fixture, &image).output().unwrap();
    let detail = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(5), "{detail}");
    assert!(detail.contains("host_upgrade_outcome_unknown"), "{detail}");
    assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
}
