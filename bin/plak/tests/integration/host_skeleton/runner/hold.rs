//! Strict placement authority cuts: real paired daemon, link, key and barrier.

use super::foreground::Paused;
use super::*;

mod support;
use support::*;

fn strict_fixture() -> Fixture {
    fixture().expect(
        "strict runner hold shape cannot skip a long Unix socket path; shorten PLAK_TEST_TMPDIR",
    )
}

fn elapsed_window(started: std::time::Instant) {
    until("independent effect window", || {
        started.elapsed() >= std::time::Duration::from_millis(1900)
    });
}

fn barrier(
    fixture: &Fixture,
    remote: &Remote,
    client: &mut Client,
    session: &str,
    paused: &mut Paused,
) {
    remote.arm("before-ready");
    paused.resume();
    remote.entered("before-ready");
    assert_waiting(fixture, client, session);
    assert!(
        matches!(
            row(client, session).runner_status,
            plakous_host_protocol::wire::RunnerStatus::Blocked { .. }
        ),
        "socket/bind released the placement before the barrier"
    );
}

fn loss_before_publication(
    fixture: &Fixture,
    remote: &Remote,
    client: &mut Client,
    session: &str,
    daemon: i32,
) {
    let mut paused = Paused::new(daemon);
    let bridge: i32 = std::fs::read_to_string(fixture.root.join("ssh-bridges"))
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .parse()
        .unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(bridge),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    // This is the retained client's actual connection observation, not socket
    // presence or a cached picker phase.
    wait_file(&remote.cuts.join("before-ready.link-lost"));
    remote.release("before-ready");
    wait_file(&remote.cuts.join("before-ready.result"));
    assert_eq!(
        std::fs::read_to_string(remote.cuts.join("before-ready.result")).unwrap(),
        "link_lost",
        "Ready ignored loss between the completed barrier and publication"
    );
    assert_waiting(fixture, client, session);
    paused.resume();
}

#[test]
fn runner_hold_poll() {
    let mut twins = Vec::new();
    for beyond_effect in [false, true] {
        let fixture = strict_fixture();
        let remote = Remote::new(&fixture);
        remote.arm("before-poll");
        fixture.write_script(&remote.script(&fixture));
        let (mut host, _attached, session, _) = remote.start(&fixture);
        remote.entered("before-poll");
        let (pid, daemon) = job(&fixture);
        let mut paused = Paused::new(daemon);
        let started = std::time::Instant::now();
        remote.release("before-poll");
        let mut admin = Client::connect(&host);
        held(&mut admin, &session);
        until("lost observation disposition", || {
            remote.cuts.join("read-interrupted").is_file()
                || unknown_effects(&fixture, &session) != 0
                || row(&mut admin, &session).activity != SessionActivity::Busy
        });
        assert_eq!(
            unknown_effects(&fixture, &session),
            0,
            "a lost repeatable observation made the live foreground unknown"
        );
        assert!(
            remote.cuts.join("read-interrupted").is_file(),
            "a lost repeatable observation ended rather than held its turn"
        );
        if beyond_effect {
            elapsed_window(started);
        }
        assert!(alive(pid), "frozen awake time was mistaken for lease lapse");
        assert_waiting(&fixture, &mut admin, &session);
        barrier(&fixture, &remote, &mut admin, &session, &mut paused);
        if beyond_effect {
            loss_before_publication(&fixture, &remote, &mut admin, &session, daemon);
        }
        assert!(
            alive(pid),
            "Acked but live foreground was cleaned during rebind"
        );
        twins.push(finish(&fixture, &remote, &mut admin, &session));
        stop(&mut host);
    }
    assert_eq!(
        twins[0], twins[1],
        "hold timing changed operation/state sets or terminal/effect counts"
    );
}

#[test]
fn runner_hold_live_key() {
    let fixture = strict_fixture();
    let remote = Remote::new(&fixture);
    remote.arm("before-answer");
    fixture.write_script(&remote.script(&fixture));
    let (mut host, _attached, session, _) = remote.start(&fixture);
    remote.entered("before-answer");
    wait_file(&fixture.root.join("local-job.marker"));
    let (pid, daemon) = job(&fixture);
    let mut paused = Paused::new(daemon);
    let started = std::time::Instant::now();
    let mut admin = Client::connect(&host);
    held(&mut admin, &session);
    elapsed_window(started);
    assert_waiting(&fixture, &mut admin, &session);
    barrier(&fixture, &remote, &mut admin, &session, &mut paused);
    assert!(alive(pid));
    finish(&fixture, &remote, &mut admin, &session);
    let requests = std::fs::read_to_string(remote.cuts.join("wire.jsonl")).unwrap();
    let effects = requests
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row["class"]["class"] == "effect_call" && row["verb"] == "process.start")
        .collect::<Vec<_>>();
    let keys = effects
        .iter()
        .map(|row| (row["class"].to_string(), row["digest"].to_string()))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), 1, "rebind invented a fresh effect key/digest");
    assert_eq!(
        effects.iter().filter(|row| row["replay"] == false).count(),
        1
    );
    assert!(
        effects
            .iter()
            .any(|row| row["replay"] == true
                && row["age_ms"].as_u64().is_some_and(|age| age >= 1600)),
        "the old key was not recovered past its original effect window"
    );
    stop(&mut host);
}

#[test]
fn runner_hold_not_sent() {
    let fixture = strict_fixture();
    let remote = Remote::new(&fixture);
    std::fs::write(remote.cuts.join("before-send.verb"), b"process.start").unwrap();
    std::fs::write(remote.cuts.join("before-send.match"), b"local-job.marker").unwrap();
    let mut script = remote.script(&fixture);
    // NotSent must precede rebind, not wait out a client read window. Keep
    // that window beyond the generous liveness guard, even under parallel load.
    script["runner_windows"]["read_ms"] = json!(30000);
    let suffix = script["steps"][0]["output"]["tool"].take();
    let mut prefix = suffix.clone();
    prefix["arguments"]["command"] = json!(format!(
        "printf prefix >> '{}'",
        fixture.root.join("prefix.marker").display()
    ));
    prefix["arguments"]["yield_time_ms"] = json!(1000);
    script["steps"][0]["output"] = json!({"tools": [prefix, suffix]});
    fixture.write_script(&script);
    let (mut host, _attached, session, _) = remote.start(&fixture);
    remote.entered("before-send");
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("prefix.marker")).unwrap(),
        "prefix"
    );
    assert!(!fixture.root.join("local-job.marker").exists());
    let mut paused = Paused::new(remote.runner.pid().unwrap());
    remote.release("before-send");
    until_within(
        "NotSent releases client gates before reconciliation",
        std::time::Duration::from_secs(5),
        || remote.cuts.join("not-sent").is_file(),
    );
    let mut admin = Client::connect(&host);
    held(&mut admin, &session);
    assert_waiting(&fixture, &mut admin, &session);
    barrier(&fixture, &remote, &mut admin, &session, &mut paused);
    let bound = std::time::Instant::now();
    until("pre-Ready read window", || {
        assert!(
            !fixture.root.join("local-job.marker").exists(),
            "NotSent retried on bind instead of Ready"
        );
        bound.elapsed() >= std::time::Duration::from_millis(750)
    });
    remote.release("before-ready");
    wait_file(&fixture.root.join("local-job.marker"));
    finish(&fixture, &remote, &mut admin, &session);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("prefix.marker")).unwrap(),
        "prefix",
        "NotSent re-entered an already completed prefix"
    );
    stop(&mut host);
}

#[test]
fn runner_hold_stop() {
    let fixture = strict_fixture();
    let remote = Remote::new(&fixture);
    remote.arm("before-poll");
    let mut script = remote.script(&fixture);
    let first = script["steps"][0]["output"]["tool"].take();
    let mut suffix = first.clone();
    suffix["arguments"]["command"] = json!(format!(
        "printf suffix > '{}'",
        fixture.root.join("forbidden-suffix").display()
    ));
    script["steps"][0]["output"] = json!({"tools": [first, suffix]});
    fixture.write_script(&script);
    let (mut host, _attached, session, _) = remote.start(&fixture);
    remote.entered("before-poll");
    let (pid, daemon) = job(&fixture);
    let mut paused = Paused::new(daemon);
    remote.release("before-poll");
    let mut admin = Client::connect(&host);
    held(&mut admin, &session);
    let (sent, response) = mpsc::channel();
    let session_to_stop = session.clone();
    std::thread::spawn(move || {
        let outcome = admin.request(
            10,
            Op::StopSession {
                session: session_to_stop,
            },
        );
        let _ = sent.send((admin, outcome));
    });
    let (mut admin, stopped_outcome) = response
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("Stop joined the placement wait instead of completing");
    assert!(matches!(
        stopped_outcome,
        Outcome::Ok(Answer::Stopped { .. })
    ));
    until_within(
        "Stop terminal while placement remains absent",
        std::time::Duration::from_secs(5),
        || row(&mut admin, &session).activity != SessionActivity::Busy,
    );
    assert_eq!(snapshot(&fixture, &session).terminals, 1);
    assert_eq!(providers(&fixture), 1);
    assert!(!fixture.root.join("forbidden-suffix").exists());
    paused.resume();
    pending_stop::ready(&mut admin, &remote.id);
    stopped(pid);
    assert!(!fixture.root.join("forbidden-suffix").exists());
    assert_eq!(
        providers(&fixture),
        1,
        "Stop admitted paid settlement/continuation"
    );
    stop(&mut host);
}

#[test]
fn runner_hold_host_loss() {
    let fixture = strict_fixture();
    let fixture = fixture.with_session_lease_seconds(2);
    let remote = Remote::new(&fixture);
    remote.arm("before-poll");
    fixture.write_script(&remote.script(&fixture));
    let (mut host, attached, session, node) = remote.start(&fixture);
    remote.entered("before-poll");
    let (pid, daemon) = job(&fixture);
    let mut paused = Paused::new(daemon);
    remote.release("before-poll");
    held(&mut Client::connect(&host), &session);
    kill(&mut host);
    drop(attached);
    let mut host = start_env(&fixture, &[], &[("PATH", remote.path.clone())]);
    let mut admin = Client::connect(&host);
    assert_eq!(providers(&fixture), 1);
    assert_ne!(
        row(&mut admin, &session).activity,
        SessionActivity::Busy,
        "a process-local hold was restored"
    );
    remote.arm("before-ready");
    paused.resume();
    remote.entered("before-ready");
    assert_eq!(providers(&fixture), 1);
    remote.release("before-ready");
    pending_stop::ready(&mut admin, &remote.id);
    admin.attach(2, &node);
    until("ordinary owner-loss recovery", || {
        root_record(&fixture, &session)
            .0
            .contains(&RootInterruptCause::OwnerLost)
    });
    assert_eq!(
        root_record(&fixture, &session).1,
        0,
        "Ready granted continuation consent"
    );
    assert_eq!(providers(&fixture), 1);
    assert_eq!(snapshot(&fixture, &session).terminals, 1);
    stopped(pid);
    stop(&mut host);
}

#[test]
fn runner_hold_identity() {
    replacement_refusal();
    let fixture = strict_fixture();
    let remote = Remote::new(&fixture);
    remote.arm("before-answer");
    fixture.write_script(&remote.script(&fixture));
    let (mut host, _attached, session, _) = remote.start(&fixture);
    remote.entered("before-answer");
    wait_file(&fixture.root.join("local-job.marker"));
    let mut asleep = Paused::new(remote.runner.pid().unwrap());
    let mut admin = Client::connect(&host);
    held(&mut admin, &session);
    asleep.resume();
    remote.runner.kill();
    // Same saved runner, new instance: its old live key is genuinely unknown.
    remote.arm("before-ready");
    remote.release("before-answer");
    remote.runner.start();
    remote.entered("before-ready");
    idle(&mut admin, &session);
    assert_eq!(
        snapshot(&fixture, &session).terminals,
        1,
        "fixed unknown waited for Ready"
    );
    assert_eq!(unknown_effects(&fixture, &session), 1);
    assert_eq!(providers(&fixture), 1, "identity loss bought paid triage");
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("local-job.marker"))
            .unwrap()
            .lines()
            .count(),
        1,
        "instance replacement re-executed the old key"
    );
    remote.release("before-ready");
    stop(&mut host);
}

fn replacement_refusal() {
    let fixture = strict_fixture();
    let remote = Remote::new(&fixture);
    std::fs::write(remote.cuts.join("before-send.verb"), b"process.start").unwrap();
    let mut script = remote.script(&fixture);
    script["runner_windows"]["read_ms"] = json!(30000);
    fixture.write_script(&script);
    let (mut host, _attached, session, _) = remote.start(&fixture);
    remote.entered("before-send");
    let mut paused = Paused::new(remote.runner.pid().unwrap());
    remote.release("before-send");
    until_within(
        "unsent identity call releases client gates before reconciliation",
        std::time::Duration::from_secs(5),
        || remote.cuts.join("not-sent").is_file(),
    );
    let mut admin = Client::connect(&host);
    held(&mut admin, &session);
    paused.resume();
    remote.runner.kill();
    std::fs::write(
        remote.runner.home.join("runner/runner-id"),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    remote.runner.start();
    idle(&mut admin, &session);
    assert_eq!(
        snapshot(&fixture, &session).terminals,
        1,
        "replaced services stranded their old waiter"
    );
    assert_eq!(providers(&fixture), 1);
    assert!(
        !fixture.root.join("local-job.marker").exists(),
        "a refused identity dispatched the held effect"
    );
    stop(&mut host);
}

#[test]
fn runner_hold_isolation() {
    let fixture = strict_fixture();
    let remote = Remote::new(&fixture);
    let config = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap().replace(
        "subagents = false",
        "subagents = false\nmax_concurrent_requests_per_account = 1",
    );
    std::fs::write(config, text).unwrap();
    remote.arm("before-poll");
    let mut script = remote.script(&fixture);
    fixture.write_script(&script);
    let (mut host, _attached, session, _) = remote.start(&fixture);
    remote.entered("before-poll");
    let (pid, daemon) = job(&fixture);
    let mut paused = Paused::new(daemon);
    remote.release("before-poll");
    let mut admin = Client::connect(&host);
    held(&mut admin, &session);
    // A second root of the same account uses the other runner. The shipped
    // spawn surface has no cross-runner child field; this is not one.
    script["steps"] = json!([{"output": {"text": "unrelated runner progressed"}}]);
    fixture.write_script(&script);
    let other_workspace = fixture.root.join("other-workspace");
    std::fs::create_dir_all(&other_workspace).unwrap();
    let mut request = new_session(&other_workspace);
    if let Op::NewSession { request_id, .. } = &mut request {
        *request_id = uuid::Uuid::new_v4().to_string();
    }
    let created = admin.request(20, request);
    let Outcome::Ok(Answer::Created { node, .. }) = created else {
        panic!("local session not created: {created:?}")
    };
    let attachment = admin.attach(21, &node);
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        admin.prompt(
            22,
            attachment,
            "one-slot sibling root",
            "unrelated runner progressed",
        );
        let _ = send.send(admin);
    });
    let progressed = receive.recv_timeout(std::time::Duration::from_secs(5));
    assert!(
        progressed.is_ok(),
        "the other runner could not complete with one shared slot"
    );
    let mut admin = progressed.unwrap();
    assert_eq!(
        providers(&fixture),
        2,
        "held placement retained the sole account request slot"
    );
    assert_eq!(snapshot(&fixture, &session).terminals, 0);
    assert_eq!(row(&mut admin, &session).activity, SessionActivity::Busy);
    assert!(alive(pid));
    paused.resume();
    pending_stop::ready(&mut admin, &remote.id);
    std::fs::write(fixture.root.join("foreground-release"), b"release").unwrap();
    idle(&mut admin, &session);
    wait_file(&fixture.root.join("foreground-completed"));
    assert_eq!(providers(&fixture), 3);
    assert_eq!(unknown_effects(&fixture, &session), 0);
    assert_eq!(snapshot(&fixture, &session).terminals, 1);
    job(&fixture);
    stop(&mut host);
}
