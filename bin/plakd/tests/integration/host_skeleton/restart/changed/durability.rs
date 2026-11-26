//! Crash the replacement on either side of runner-line transfer.

use super::*;
use plakous_hosted::test_support_runner::HostCut;
use std::os::unix::process::ExitStatusExt as _;

#[test]
fn changed_local_triple_recovers_after_readoption_cuts() {
    readoption_cuts(false);
}

#[test]
fn changed_paired_triple_recovers_after_readoption_cuts() {
    readoption_cuts(true);
}

#[test]
fn losing_the_original_daemon_on_either_side_of_transfer_never_replays() {
    for paired in [false, true] {
        for cut in [HostCut::Lease, HostCut::Transfer] {
            crash(paired, cut, true);
        }
    }
}

fn readoption_cuts(paired: bool) {
    for cut in [HostCut::Lease, HostCut::Transfer, HostCut::Terminals] {
        crash(paired, cut, false);
    }
}

fn crash(remote: bool, cut: HostCut, lose_original: bool) {
    let Some(fixture) = fixture() else { return };
    let fixture = fixture.with_session_lease_seconds(2);
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let paired = remote.then(|| Paired::new(&fixture));
    let mut environment = candidate_environment(&image).to_vec();
    if let Some(paired) = &paired {
        environment.push(("PATH", paired.path.clone()));
    }

    source_script(&fixture);
    let mut host = start_env(&fixture, &[], &environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    if let Some(paired) = &paired {
        runner::remote_ready(&mut client, &paired.id);
    }
    let root = start_root(
        &fixture,
        &mut client,
        "carried",
        paired.as_ref().map(|p| p.id.as_str()),
        1,
    );
    drop(client);
    destination_script(&fixture);
    if let Some(paired) = &paired {
        paired.advance(&image);
    }
    stop(&mut host);
    assert!(runner::alive(root.background.0));
    wait_exit(root.foreground.0);
    let original = lose_original.then(|| original_lock(&fixture, paired.as_ref()));
    let finish = fixture.root.join("carried-bg-finish");
    if cut == HostCut::Terminals {
        std::fs::write(&finish, "").unwrap();
        wait_exit(root.background.0);
    }

    let (mut candidate, _) = spawn_host(
        image_command(&fixture, &image)
            .envs(environment.iter().cloned())
            .env("PLAK_TEST_HOST_READOPT_CUT", cut.name())
            .arg("host"),
    );
    assert_eq!(
        runner::cut_exit(&mut candidate).signal(),
        Some(9),
        "{cut:?}"
    );
    assert_eq!(marked(&fixture, &root.name, "bg"), root.background);
    if cut != HostCut::Terminals {
        assert!(runner::alive(root.background.0));
    }
    if let Some(lock) = original {
        kill_original(&lock, root.background.1);
    }

    let mut host = start_image(&fixture, &image, &environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    if let Some(paired) = &paired {
        runner::remote_ready(&mut client, &paired.id);
    }
    client.attach(20, &root.node);
    std::fs::write(finish, "").unwrap();
    release_continuation(&fixture);
    wait_exit(root.background.0);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let deadline = Instant::now() + WAIT;
    loop {
        let (head, _) = identity(&fixture, &root.session);
        let delivered = inspect_process_notices(&paths.runtime_database(), head).unwrap();
        let count = delivered.iter().filter(|job| **job == 1).count();
        if count != 0 {
            assert_eq!(count, 1, "{cut:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "retained terminal missing after {cut:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(marked(&fixture, &root.name, "bg"), root.background);
    drop(client);
    stop(&mut host);
    let (head, _) = identity(&fixture, &root.session);
    let database = paths.runtime_database();
    let delivered = inspect_process_notices(&database, head).unwrap();
    assert_eq!(delivered.iter().filter(|job| **job == 1).count(), 1);
    assert_terminal(&database, head, lose_original);
}

fn assert_terminal(database: &Path, head: plakous::sessions::SessionLocator, lost: bool) {
    let checkpoint =
        plakous_offline_crash_harness::ledger_signature::inspect_branch_checkpoint(database, head)
            .unwrap();
    let notice = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(plakous::model::HistorySegment::items)
        .flat_map(plakous::model::ConversationItem::blocks)
        .filter_map(|block| match block {
            plakous::model::ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .filter(|text| text.contains("<process_notice "))
        .collect::<Vec<_>>()
        .join("\n");
    let expected = if lost {
        "status=\"outcome_unknown\""
    } else {
        "bg-retained-output"
    };
    assert!(notice.contains(expected), "{notice}");
}

fn original_lock(fixture: &Fixture, paired: Option<&Paired>) -> PathBuf {
    let Some(paired) = paired else {
        return local_runner_root(&fixture.home()).join("run/r.lock");
    };
    let mut locks = std::fs::read_dir(paired.runner.home.join("runner/builds"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("run"))
        .flat_map(|run| std::fs::read_dir(run).unwrap())
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "lock"));
    let lock = locks.next().unwrap();
    assert!(locks.next().is_none());
    lock
}

fn kill_original(path: &Path, daemon: i32) {
    let lock = std::fs::File::options().write(true).open(path).unwrap();
    let pid = rustix::process::Pid::from_raw(daemon).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                assert!(Instant::now() < deadline, "old daemon still owns its lock");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot inspect the old daemon lock: {error}"),
        }
    }
}
