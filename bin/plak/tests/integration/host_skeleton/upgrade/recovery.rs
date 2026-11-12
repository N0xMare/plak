//! Plain host startup must withdraw preparation, never manufacture surrender.

use super::*;

fn plain_start_cut(cut: &str, destination: bool) {
    start_cut(cut, destination, false);
}

fn plain_destination(fixture: &Fixture, image: &std::path::Path) -> Host {
    let mut child = Child::spawn(
        image_command(fixture, image)
            .env("PLAK_TEST_STATE_KEY", DESTINATION)
            .arg("host")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    );
    let stderr = child.stderr.take().unwrap();
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = send.send(line);
        }
    });
    serving_host((child, lines)).expect("plain destination startup")
}

fn start_cut(cut: &str, destination: bool, missing_image: bool) {
    let Some(fixture) = fixture() else { return };
    let image = replacement_image(&fixture);
    let probe = image_command(&fixture, &image)
        .env("PLAK_TEST_STATE_KEY", DESTINATION)
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success());
    let probe: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let binary = probe["binary_id"].as_str().unwrap();
    assert_ne!(binary, super::binary(&fixture));
    let sources = [seed(&fixture, 21), seed(&fixture, 22)];
    let mut host = start_env(&fixture, &[], &[("PLAK_TEST_HOST_UPGRADE_CUT", cut.into())]);
    let _ = image_command(&fixture, &image)
        .env("PLAK_TEST_STATE_KEY", DESTINATION)
        .args(["host", "upgrade", "--expect-binary", binary])
        .output()
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while host.child.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(receipts(&fixture).pop().unwrap()["phase"], "prepared");
    if missing_image {
        let mut receipt = receipts(&fixture).pop().unwrap();
        receipt["source_executable"] = json!(fixture.root.join("missing-source-snapshot"));
        let file = fixture
            .home()
            .join("host/upgrades")
            .join(receipt["id"].as_str().unwrap())
            .join("receipt.json");
        write_private(&file, &receipt.to_string());
    }
    let mut restored = if destination {
        plain_destination(&fixture, &image)
    } else {
        start(&fixture)
    };
    if missing_image {
        assert!(restored.socket.ends_with(format!("{DESTINATION}.sock")));
        assert_eq!(receipts(&fixture).pop().unwrap()["phase"], "aborted");
        stop(&mut restored);
        restored = start(&fixture);
    }
    assert_eq!(receipts(&fixture).pop().unwrap()["phase"], "aborted");
    let mut client = Client::connect(&restored);
    let rows = client.rows(1);
    for source in &sources {
        let row = rows.iter().find(|row| row.session == *source).unwrap();
        let attachment = client.attach(2, &row.node);
        assert_eq!(
            client.request(3, Op::Detach { attachment }),
            Outcome::Ok(Answer::Detached)
        );
    }
    let database = fixture
        .home()
        .join(format!("state/{DESTINATION}/product.sqlite3"));
    if database.exists() {
        let connection = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    stop(&mut restored);
    // An already withdrawn receipt is inert, even for the former destination.
    let mut independent = plain_destination(&fixture, &image);
    assert!(independent.socket.ends_with(format!("{DESTINATION}.sock")));
    assert_eq!(Client::connect(&independent).rows(1), []);
    stop(&mut independent);
}

#[test]
fn host_upgrade_plain_source_start_withdraws_after_receipt() {
    plain_start_cut("after-receipt", false);
}
#[test]
fn host_upgrade_plain_destination_start_withdraws_after_receipt() {
    plain_start_cut("after-receipt", true);
}
#[test]
fn host_upgrade_plain_source_start_withdraws_after_mapping() {
    plain_start_cut("after-mapping", false);
}
#[test]
fn host_upgrade_plain_destination_start_withdraws_after_mapping() {
    plain_start_cut("after-mapping", true);
}
#[test]
fn host_upgrade_plain_start_restores_sources_after_archival_before_surrender() {
    plain_start_cut("after-source-archive", true);
}

#[test]
fn host_upgrade_missing_prepared_source_image_does_not_prevent_host_start() {
    start_cut("after-source-archive", true, true);
}

#[test]
fn host_upgrade_preparation_error_withdraws_without_a_restart() {
    let Some(fixture) = fixture() else { return };
    let sources = [seed(&fixture, 31), seed(&fixture, 32)];
    let mut host = start_env(
        &fixture,
        &[],
        &[(
            "PLAK_TEST_HOST_UPGRADE_PREPARE_FAILURE",
            "after-mapping".into(),
        )],
    );
    let refused = upgrade(&fixture, &binary(&fixture));
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("injected host upgrade preparation failure")
    );
    assert_eq!(receipts(&fixture).pop().unwrap()["phase"], "aborted");
    let mut client = Client::connect(&host);
    let rows = client.rows(1);
    for source in sources {
        let row = rows.iter().find(|row| row.session == source).unwrap();
        client.attach(2, &row.node);
    }
    stop(&mut host);
}
