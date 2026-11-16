//! Durable remote witnesses are checked before any destination fence advances.
use super::*;
use plakous::runner::{FileOwnerFloors, Floor, OwnerFloors as _};

#[derive(Clone, Copy, Debug)]
enum Witness {
    Foreign,
    Lower,
    Higher,
}

#[test]
fn host_move_remote_foreign_owner_withholds_every_connected_member_without_a_new_fence() {
    witness(Witness::Foreign);
}

#[test]
fn host_move_remote_lower_owner_withholds_every_connected_member_without_a_new_fence() {
    witness(Witness::Lower);
}

#[test]
fn host_move_remote_higher_owner_withholds_every_connected_member_without_a_new_fence() {
    witness(Witness::Higher);
}

#[test]
fn host_move_remote_outage_keeps_its_published_group_while_an_independent_local_group_is_ready() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (daemon, path, runner_id) = runner::remote_fixture(&fixture);
    fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
    let workspace = fixture.root.join("local-workspace");
    std::fs::create_dir(&workspace).unwrap();
    let local = seed_in(&fixture, 111, &workspace);
    let remote = remote::seed(&fixture, &path, &runner_id, 112);
    let mut source = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PATH", path.clone()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-commit".into()),
        ],
    );
    runner::remote_ready(&mut Client::connect(&source), &runner_id);
    cut_source(&fixture, &mut source, &path);
    let mut target = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PATH", path.clone()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
            ("PLAK_TEST_HOST_UPGRADE_FAILURE", "reconcile-pause".into()),
        ],
    );
    wait_groups(&fixture, |groups| {
        groups.iter().all(|group| group["phase"] == "published")
    });
    for group in receipts(&fixture)[0]["groups"].as_array().unwrap() {
        review::assert_retired(&fixture, group);
    }
    stop(&mut target);
    let offline = fixture.root.join("runner-offline");
    let ssh = fixture.root.join("ssh-bin/ssh");
    let bridge = std::fs::read_to_string(&ssh).unwrap();
    std::fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n[ ! -f '{}' ] || exit 70\n{}",
            offline.display(),
            bridge.strip_prefix("#!/bin/sh\n").unwrap()
        ),
    )
    .unwrap();
    std::fs::write(&offline, "offline").unwrap();
    daemon.kill();
    let mut target = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PATH", path.clone()),
            ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
        ],
    );
    wait_groups(&fixture, |groups| {
        groups.iter().any(|group| {
            group["products"]
                .as_array()
                .unwrap()
                .contains(&json!(local))
                && group["phase"] == "ready"
        })
    });
    let receipt = receipts(&fixture).remove(0);
    assert_eq!(receipt["phase"], "committed");
    let remote_group = receipt["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| {
            group["products"]
                .as_array()
                .unwrap()
                .contains(&json!(remote))
        })
        .unwrap();
    assert_eq!(remote_group["phase"], "published");
    assert!(remote_group["transfers"].as_object().unwrap().is_empty());
    let mut client = Client::connect(&target);
    let rows = client.rows(1);
    assert!(rows.iter().any(|row| row.session == local));
    assert!(!rows.iter().any(|row| row.session == remote));
    std::fs::remove_file(offline).unwrap();
    daemon.start();
    runner::remote_ready(&mut client, &runner_id);
    wait_groups(&fixture, |groups| {
        groups.iter().all(|group| group["phase"] == "ready")
    });
    assert_eq!(receipts(&fixture)[0]["phase"], "acknowledged");
    assert_eq!(successor(&fixture, &remote), remote);
    assert_eq!(successor(&fixture, &local), local);
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut target);
}

fn wait_groups(fixture: &Fixture, ready: impl Fn(&[serde_json::Value]) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let receipt = receipts(fixture).remove(0);
        if ready(receipt["groups"].as_array().unwrap()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "groups did not reach their required dispositions: {receipt:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn witness(kind: Witness) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (daemon, path, runner_id) = runner::remote_fixture(&fixture);
    fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
    let source = remote::seed(&fixture, &path, &runner_id, 101);
    let mut host = start_env(
        &fixture,
        &[],
        &[(ENABLED, "1".into()), ("PATH", path.clone())],
    );
    let fork = fork_remote(&fixture, &host, &source);
    stop(&mut host);
    let mut host = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PATH", path.clone()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-publish".into()),
        ],
    );
    runner::remote_ready(&mut Client::connect(&host), &runner_id);
    cut_source(&fixture, &mut host, &path);
    let receipt = receipts(&fixture).remove(0);
    let group = &receipt["groups"][0];
    assert_eq!(group["trees"].as_array().unwrap().len(), 2);
    assert_eq!(group["lines"].as_array().unwrap().len(), 2);
    review::assert_retired(&fixture, group);
    let floors = remote_floors(&daemon);
    let injected = inject(&floors, group, kind);
    let before = fences(&fixture);
    for _ in 0..2 {
        let mut target = start_env(
            &fixture,
            &[],
            &[
                (ENABLED, "1".into()),
                ("PATH", path.clone()),
                ("PLAK_TEST_STATE_KEY", DESTINATION.into()),
            ],
        );
        wait_foreign_witness(&target);
        let receipt = receipts(&fixture).remove(0);
        let group = &receipt["groups"][0];
        assert_eq!(group["phase"], "published", "{kind:?}");
        assert!(group["transfers"].as_object().unwrap().is_empty());
        let rows = Client::connect(&target).rows(1);
        for member in [&source, &fork] {
            assert!(
                !rows.iter().any(|row| &row.session == member),
                "partial connected-group visibility"
            );
        }
        assert_eq!(
            fences(&fixture),
            before,
            "{kind:?} witness minted a native fence"
        );
        assert!(
            FileOwnerFloors::open(floors.path())
                .unwrap()
                .load()
                .contains(&injected)
        );
        stop(&mut target);
    }
}

fn inject(floors: &FileOwnerFloors, group: &serde_json::Value, kind: Witness) -> (String, Floor) {
    let (host, mut floor) = floors
        .load()
        .into_iter()
        .find(|(_, floor)| {
            group["lines"]
                .as_array()
                .unwrap()
                .contains(&json!(floor.owner.line))
        })
        .unwrap();
    match kind {
        Witness::Foreign => floor.owner.branch = uuid::Uuid::new_v4().to_string(),
        Witness::Lower => {
            assert!(
                floor.owner.fence > 1,
                "fixture needs a positive lower fence"
            );
            floor.owner.fence -= 1;
        }
        Witness::Higher => floor.owner.fence += 100,
    }
    floors.record(&host, &floor).unwrap();
    (host, floor)
}

fn fork_remote(fixture: &Fixture, host: &Host, source: &str) -> String {
    let mut client = Client::connect(host);
    let node = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == source)
        .unwrap()
        .node;
    let attachment = client.attach(2, &node);
    delete_paused(fixture, &mut client, source, attachment);
    assert_eq!(
        client.request(
            3,
            Op::Intent {
                attachment,
                intent: SessionIntent::ForkSession {
                    account: None,
                    compact: false
                },
                client_id: None,
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(row) = client.rows(4).into_iter().find(|row| row.session != source) {
            return row.session;
        }
        assert!(Instant::now() < deadline, "remote fork did not settle");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cut_source(fixture: &Fixture, host: &mut Host, path: &str) {
    let result = output_env(
        fixture,
        &["host", "upgrade", "--expect-binary", &binary(fixture)],
        true,
        &[(ENABLED, "1"), ("PATH", path)],
    );
    assert!(!result.status.success());
    assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
    assert_eq!(receipts(fixture)[0]["phase"], "committed");
}

fn remote_floors(daemon: &ChildRunner) -> FileOwnerFloors {
    let paths = std::fs::read_dir(daemon.home.join("runner/builds"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("owners.json"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert_eq!(paths.len(), 1);
    FileOwnerFloors::open(&paths[0]).unwrap()
}

pub(super) fn fences(fixture: &Fixture) -> Vec<(String, Vec<u8>)> {
    let connection = rusqlite::Connection::open_with_flags(
        fixture
            .home()
            .join("state")
            .join(DESTINATION)
            .join("runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    connection
        .prepare("SELECT branch_id,last_fence FROM branches ORDER BY branch_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn wait_foreign_witness(host: &Host) {
    let deadline = Instant::now() + WAIT;
    loop {
        let line = host
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("destination did not report the real foreign witness");
        if line.contains("foreign witness for line") {
            return;
        }
    }
}
