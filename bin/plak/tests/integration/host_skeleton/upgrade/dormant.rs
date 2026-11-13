//! The complete dormant ownership path is qualified behind its private test capability.
use super::*;
use plak::store::session_records::SessionRecordStore as _;
use plakous_host_protocol::intent::SessionIntent;

const ENABLED: &str = "PLAK_TEST_HOST_UPGRADE_DORMANT";

mod children;
mod review;

#[test]
fn host_move_dormant_round_trip_preserves_ids_and_withholds_until_group_ready() {
    round_trip(None);
}

#[test]
fn host_move_dormant_committed_source_recovers_native_retirement_before_import() {
    round_trip(Some("move-after-commit"));
}

#[test]
fn host_move_dormant_partial_native_stage_and_publication_recover_once() {
    for cut in [
        "move-after-stage",
        "move-before-stage",
        "move-after-product-seed",
        "move-after-publish",
        "move-after-transfer",
    ] {
        round_trip(Some(cut));
    }
}

#[test]
fn host_move_dormant_prepared_native_freeze_is_withdrawn_by_plain_source_start() {
    for cut in [
        "move-after-receipt",
        "move-after-cleanup-intent",
        "move-after-source-prepare",
        "move-after-group-export",
        "move-after-source-archive",
    ] {
        prepared_cut(cut);
    }
}

#[test]
fn host_move_dormant_retirement_transfer_and_ready_cuts_recover_once() {
    for cut in [
        "move-after-native-retirement",
        "move-before-transfer",
        "move-before-ready",
        "move-after-ready",
    ] {
        round_trip(Some(cut));
    }
}

#[test]
fn host_move_dormant_local_and_remote_groups_reconcile_on_the_registry_connections() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (_daemon, path, runner_id) = runner::remote_fixture(&fixture);
    fixture.write_script(&json!({"version":1, "steps":[{"output":{"text":ANSWER}}]}));
    let local_workspace = fixture.root.join("local-workspace");
    std::fs::create_dir(&local_workspace).unwrap();
    let local = seed_in(&fixture, 31, &local_workspace);
    let remote = remote::seed(&fixture, &path, &runner_id, 32);
    let mut host = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PATH", path.clone()),
            ("PLAK_TEST_HOST_UPGRADE_FAILURE", "probe-idle-window".into()),
        ],
    );
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &runner_id);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1"), ("PATH", &path)],
    );
    assert!(
        result.status.success(),
        "{}\n{:?}",
        String::from_utf8_lossy(&result.stderr),
        host.lines.try_iter().collect::<Vec<_>>()
    );
    let authority = receipts(&fixture);
    assert_eq!(authority[0]["phase"], "acknowledged");
    assert_eq!(authority[0]["groups"].as_array().unwrap().len(), 2);
    for source in [&local, &remote] {
        assert_eq!(successor(&fixture, source), *source);
        identity(&fixture, DESTINATION, source);
    }
    let document = remote::export(&fixture, &path, &remote, "moved-remote.json");
    assert_eq!(document["queue"].as_array().unwrap().len(), 1);
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut host);
}

#[test]
fn host_move_dormant_shared_forks_publish_together_while_an_unrelated_tree_is_independent() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let source = seed(&fixture, 41);
    let independent = seed(&fixture, 42);
    let mut host = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    client.retain_observations();
    let node = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == source)
        .unwrap()
        .node;
    let attachment = client.attach(2, &node);
    delete_paused(&fixture, &mut client, &source, attachment);
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
    let fork = loop {
        if let Some(row) = client
            .rows(4)
            .into_iter()
            .find(|row| row.session != source && row.session != independent)
        {
            break row.session;
        }
        assert!(
            Instant::now() < deadline,
            "source did not finish the fork: {:?}",
            client.retained
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let native_source = identity(&fixture, &plak::home::state::state_key(), &source);
    let native_fork = identity(&fixture, &plak::home::state::state_key(), &fork);
    assert_eq!(native_source.0.session_id(), native_fork.0.session_id());
    assert_ne!(native_source.1, native_fork.1);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let authority = receipts(&fixture);
    assert_eq!(authority[0]["phase"], "acknowledged");
    let groups = authority[0]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    let linked = groups
        .iter()
        .find(|group| group["products"].as_array().unwrap().len() == 2)
        .unwrap();
    let progress = String::from_utf8_lossy(&result.stderr);
    assert!(progress.contains(linked["id"].as_str().unwrap()));
    for tree in linked["trees"].as_array().unwrap() {
        assert!(progress.contains(tree.as_str().unwrap()));
    }
    assert_eq!(linked["trees"].as_array().unwrap().len(), 2);
    assert!(
        linked["products"]
            .as_array()
            .unwrap()
            .contains(&json!(source))
    );
    assert!(
        linked["products"]
            .as_array()
            .unwrap()
            .contains(&json!(fork))
    );
    for product in [&source, &fork, &independent] {
        assert_eq!(successor(&fixture, product), *product);
        export(&fixture, product, true, &format!("moved-{product}.json"));
    }
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut host);
}

fn delete_paused(fixture: &Fixture, client: &mut Client, source: &str, attachment: u64) {
    let record = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    )
    .load(source)
    .unwrap();
    let queued = &record.pending_queue[0];
    assert_eq!(
        client.request(
            5,
            Op::Intent {
                attachment,
                intent: SessionIntent::DeleteQueued {
                    id: queued.id.clone(),
                    revision: queued.revision,
                },
                client_id: None,
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
}

fn prepared_cut(cut: &str) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let ids = [seed(&fixture, 21), seed(&fixture, 22)];
    let mut host = start_env(
        &fixture,
        &[],
        &[
            (ENABLED, "1".into()),
            ("PLAK_TEST_HOST_UPGRADE_CUT", cut.into()),
        ],
    );
    runner::wait_local_runner_ready(&host);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!result.status.success());
    let authority = receipts(&fixture);
    assert_eq!(authority[0]["phase"], "prepared");
    assert!(host.child.as_mut().unwrap().try_wait().unwrap().is_some());
    let mut restored = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
    assert_eq!(receipts(&fixture)[0]["phase"], "aborted");
    for id in ids {
        let document = export(&fixture, &id, false, &format!("restored-{id}.json"));
        assert_eq!(document["queue"].as_array().unwrap().len(), 1);
    }
    assert!(
        !fixture
            .home()
            .join(format!("state/{DESTINATION}/product.sqlite3"))
            .exists()
    );
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut restored);
    assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
}

fn round_trip(cut: Option<&str>) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let ids = [seed(&fixture, 17), seed(&fixture, 18)];
    let before = ids
        .iter()
        .map(|id| export(&fixture, id, false, &format!("before-{id}.json")))
        .collect::<Vec<_>>();
    let native_before = ids
        .iter()
        .map(|id| identity(&fixture, &plak::home::state::state_key(), id))
        .collect::<Vec<_>>();
    let mut environment = vec![(ENABLED, "1".into())];
    if let Some(cut) = cut {
        environment.push(("PLAK_TEST_HOST_UPGRADE_CUT", cut.into()));
    }
    let mut host = start_env(&fixture, &[], &environment);
    runner::wait_local_runner_ready(&host);
    let id = binary(&fixture);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &id],
        true,
        &[(ENABLED, "1")],
    );
    if let Some(cut) = cut {
        assert!(!result.status.success());
        recover_committed_cut(&fixture, &mut host, &id, cut);
    } else {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let authority = receipts(&fixture);
    assert_eq!(authority[0]["phase"], "acknowledged");
    assert_eq!(authority[0]["groups"].as_array().unwrap().len(), 2);
    for (index, source) in ids.iter().enumerate() {
        assert_eq!(successor(&fixture, source), *source);
        let (locator, line) = identity(&fixture, DESTINATION, source);
        assert_eq!(locator.session_id(), native_before[index].0.session_id());
        assert_eq!(locator.branch_id(), native_before[index].0.branch_id());
        assert_eq!(line, native_before[index].1);
        assert_ne!(
            locator.checkpoint_id(),
            native_before[index].0.checkpoint_id()
        );
        let document = export(&fixture, source, true, &format!("after-{source}.json"));
        assert_eq!(document["named_state"], before[index]["named_state"]);
        assert_eq!(document["queue"], before[index]["queue"]);
        let mapping = authority[0]["mappings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mapping| mapping["source"] == *source)
            .unwrap();
        assert_eq!(mapping["source"], mapping["successor"]);
        assert_eq!(mapping["ready"], true);
        let refused = output(
            &fixture,
            &["run", "--session", source, "--", "must not run"],
            false,
        );
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("source_fenced")
                || String::from_utf8_lossy(&refused.stderr).contains("session_archived")
        );
    }
    for group in authority[0]["groups"].as_array().unwrap() {
        assert_eq!(group["source_retired"], true);
        review::assert_retired(&fixture, group);
        assert_eq!(group["phase"], "ready");
        assert!(!group["transfers"].as_object().unwrap().is_empty());
    }
    ready_cannot_abandon(&fixture, &ids[0]);
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    if cut.is_some() {
        assert_eq!(host.child.take().unwrap().wait().unwrap().code(), Some(86));
    } else {
        stop(&mut host);
    }
}

fn recover_committed_cut(fixture: &Fixture, host: &mut Host, id: &str, cut: &str) {
    let deadline = Instant::now() + WAIT;
    while host.child.as_mut().unwrap().try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let committed = receipts(fixture);
    assert_eq!(committed[0]["phase"], "committed");
    if cut == "move-before-stage" {
        assert!(
            committed[0]["groups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|group| group["target_digest"].is_string())
        );
    }
    let recovered = output_env(
        fixture,
        &["host", "upgrade", "--expect-binary", id],
        true,
        &[(ENABLED, "1")],
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
}

fn ready_cannot_abandon(fixture: &Fixture, id: &str) {
    let refused = output_env(
        fixture,
        &["host", "upgrade", "--abandon", id],
        true,
        &[(ENABLED, "1")],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Ready or foreign groups"));
    assert_eq!(receipts(fixture)[0]["phase"], "acknowledged");
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
