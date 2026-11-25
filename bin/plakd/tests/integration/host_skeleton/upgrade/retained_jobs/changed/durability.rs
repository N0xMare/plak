//! Crash cuts straddle both the host's release and the receiver's owner CAS.

use super::*;

#[test]
fn crash_after_child_handoffs_continues_the_child_and_holds_the_root() {
    finite_drain::crash_after_child_records();
}
use plakous_hosted::test_support_runner::HostCut;

#[test]
fn resume_after_each_handoff_boundary_preserves_original_jobs() {
    handoff_cuts(false);
}

#[test]
fn paired_resume_after_each_handoff_boundary_preserves_original_jobs() {
    handoff_cuts(true);
}

fn handoff_cuts(paired: bool) {
    for (flag, cut) in [
        ("PLAK_TEST_HOST_UPGRADE_CUT", "before-answer"),
        ("PLAK_TEST_HOST_UPGRADE_CUT", "after-source-release"),
        (
            "PLAK_TEST_HOST_UPGRADE_CUT",
            "after-destination-verification",
        ),
        ("PLAK_TEST_HOST_READOPT_CUT", HostCut::Lease.name()),
        ("PLAK_TEST_HOST_READOPT_CUT", HostCut::Transfer.name()),
        ("PLAK_TEST_HOST_READOPT_CUT", HostCut::Terminals.name()),
    ] {
        crash(flag, cut, false, paired);
    }
}

#[test]
fn original_daemon_loss_on_both_sides_of_transfer_is_unknown_and_never_replays() {
    daemon_loss(false);
}

#[test]
fn paired_original_daemon_loss_on_both_sides_of_transfer_is_unknown_and_never_replays() {
    daemon_loss(true);
}

fn daemon_loss(paired: bool) {
    crash(
        "PLAK_TEST_HOST_UPGRADE_CUT",
        "after-source-release",
        true,
        paired,
    );
    crash(
        "PLAK_TEST_HOST_READOPT_CUT",
        HostCut::Transfer.name(),
        true,
        paired,
    );
}

fn crash(flag: &str, cut: &str, lose_original: bool, paired: bool) {
    let Some(fixture) = fixture() else { return };
    let fixture = fixture.with_session_lease_seconds(2);
    let _cleanup = HomeHost(fixture.home());
    let paired = paired.then(|| Paired::new(&fixture));
    let config = fixture.home().join("config.toml");
    let prior = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        config,
        format!("{prior}\n[host]\nresume_after_loss = \"continue\"\n"),
    )
    .unwrap();
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let mut environment = candidate_environment(&image).to_vec();
    if let Some(paired) = &paired {
        environment.push(("PATH", paired.path.clone()));
    }
    let mut source_environment = environment.clone();
    source_environment.push((flag, cut.into()));
    source_script(&fixture);
    let mut host = start_env(&fixture, &[], &source_environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    let placement = paired.as_ref().map(|paired| {
        runner::remote_ready(&mut client, &paired.id);
        paired.id.as_str()
    });
    let root = start_root(&fixture, &mut client, "carried", placement, 1);
    let lock = original_lock(&fixture, paired.as_ref());
    drop(client);
    destination_script(&fixture);
    if let Some(paired) = &paired {
        paired.advance(&image);
    }
    let mut requester = TestChild::spawn(
        candidate_upgrade(&fixture, &image, &environment)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    let status = runner::cut_exit(host.child.as_mut().unwrap());
    assert!(!status.success(), "cut {cut} exited normally");
    let _ = requester.kill();
    let _ = requester.wait();
    assert_eq!(marked(&fixture, &root.name, "bg"), root.background, "{cut}");
    assert_eq!(marked(&fixture, &root.name, "fg"), root.foreground, "{cut}");
    assert!(
        runner::alive(root.background.0) && runner::alive(root.foreground.0),
        "{cut}"
    );
    if lose_original {
        // Leave the original's stale socket in place, as SIGKILL would.
        let daemon = rustix::process::Pid::from_raw(root.foreground.1).unwrap();
        rustix::process::kill_process(daemon, rustix::process::Signal::KILL).unwrap();
        let lock = std::fs::File::options().write(true).open(lock).unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    assert!(Instant::now() < deadline, "old daemon still holds its lock");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("cannot inspect the old daemon lock: {err}"),
            }
        }
    }
    if cut == "after-destination-verification" {
        let restart = image_command(&fixture, &image)
            .arg("host")
            .output()
            .unwrap();
        assert_eq!(restart.status.code(), Some(5), "{restart:?}");
        assert!(
            String::from_utf8_lossy(&restart.stderr)
                .contains("inherited upgrade did not reach Ready")
        );
        // The crash is the injected cause; the operator acknowledges it
        // before restarting the destination on the unchanged stores.
        let status = image_command(&fixture, &image)
            .args(["host", "upgrade-verdict"])
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
        let verdict: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(verdict["state"], "pending");
        let acknowledge = || {
            image_command(&fixture, &image)
                .args([
                    "host",
                    "upgrade-verdict",
                    "--acknowledge",
                    verdict["id"].as_str().unwrap(),
                ])
                .output()
                .unwrap()
        };
        let db = plakd::home::paths::PlakPaths::explicit(fixture.home()).runtime_database();
        let displaced = db.with_extension("displaced");
        std::fs::rename(&db, &displaced).unwrap();
        let refused = acknowledge();
        std::fs::rename(&displaced, &db).unwrap();
        assert!(
            !refused.status.success()
                && String::from_utf8_lossy(&refused.stderr)
                    .contains("host_upgrade_acknowledge_wrong_state"),
            "{refused:?}"
        );
        let runtime = rusqlite::Connection::open(&db).unwrap();
        let original: Vec<u8> = runtime
            .query_row(
                "SELECT lease_domain FROM store_meta WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        runtime
            .execute(
                "UPDATE store_meta SET lease_domain=randomblob(48) WHERE id=1",
                [],
            )
            .unwrap();
        let mismatch = acknowledge();
        let detail = String::from_utf8_lossy(&mismatch.stderr);
        assert_eq!(mismatch.status.code(), Some(5), "{detail}");
        assert!(
            detail.contains("host_upgrade_acknowledge_identity_mismatch: the runtime store"),
            "{detail}"
        );
        runtime
            .execute(
                "UPDATE store_meta SET lease_domain=?1 WHERE id=1",
                [original],
            )
            .unwrap();
        drop(runtime);
        let acknowledge = acknowledge();
        assert!(acknowledge.status.success(), "{acknowledge:?}");
    }
    let mut replacement = await_serving_host(spawn_host(
        image_command(&fixture, &image)
            .envs(environment.iter().cloned())
            .arg("host"),
    ))
    .unwrap();
    runner::wait_local_runner_ready(&replacement);
    if !lose_original {
        for job in ["bg", "fg"] {
            std::fs::write(fixture.root.join(format!("{}-{job}-finish", root.name)), "").unwrap();
        }
        wait_exit(root.background.0);
        wait_exit(root.foreground.0);
    }
    release_continuation(&fixture);
    // The pre-Ready crash has no planned release record for the start scan.
    // Reopening supplies the ordinary loss-recovery entry point.
    let mut client = Client::connect(&replacement);
    if let Some(paired) = &paired {
        runner::remote_ready(&mut client, &paired.id);
    }
    client.attach(20, &root.node);
    wait_notices(&fixture, &root.session, cut);
    assert_eq!(marked(&fixture, &root.name, "bg"), root.background);
    assert_eq!(marked(&fixture, &root.name, "fg"), root.foreground);
    drop(client);
    stop(&mut replacement);
    notices(&fixture, &root.session, !lose_original, cut);
}

fn original_lock(fixture: &Fixture, paired: Option<&Paired>) -> PathBuf {
    let Some(paired) = paired else {
        return local_runner_root(&fixture.home()).join("run/r.lock");
    };
    let directory = paired.runner.home.join("runner/builds");
    let mut locks = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("run"))
        .flat_map(|run| std::fs::read_dir(run).unwrap())
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "lock"));
    let lock = locks.next().unwrap();
    assert!(locks.next().is_none(), "more than one original native lock");
    lock
}
