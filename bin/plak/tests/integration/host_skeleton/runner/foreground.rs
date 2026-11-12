//! Real-host runner wake cuts: frozen awake time, live calls, and lease lapse.

use super::*;

pub(super) struct Paused(i32);

impl Paused {
    pub(super) fn new(pid: i32) -> Self {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGSTOP,
        )
        .unwrap();
        Self(pid)
    }
    pub(super) fn resume(&mut self) {
        if self.0 != 0 {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.0),
                nix::sys::signal::Signal::SIGCONT,
            )
            .unwrap();
            self.0 = 0;
        }
    }
}

impl Drop for Paused {
    fn drop(&mut self) {
        self.resume();
    }
}

fn clock(fixture: &Fixture, milliseconds: u64) {
    let directory = fixture.home().join(format!(
        "host/runner/v{}",
        plakous::runner::protocol::PROTOCOL_VERSION
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let temporary = directory.join("lease-clock-next");
    std::fs::write(&temporary, milliseconds.to_string()).unwrap();
    std::fs::rename(temporary, directory.join("lease-clock-ms")).unwrap();
}

#[derive(Clone, Copy)]
enum WakeMode {
    Settled,
    Live,
}

fn begin(fixture: &Fixture, mode: WakeMode) -> (Host, Client, String) {
    begin_env(fixture, mode, &[])
}

fn begin_env(fixture: &Fixture, mode: WakeMode, env: &[(&str, String)]) -> (Host, Client, String) {
    clock(fixture, 0);
    let window = match mode {
        WakeMode::Settled => 1200,
        WakeMode::Live => 10000,
    };
    let mut script = job_script(fixture, false);
    script["runner_windows"] =
        json!({"read_ms": window, "effect_ms": window, "ping_ms": 100, "dead_ms": 500});
    if matches!(mode, WakeMode::Settled) {
        let cut = fixture.root.join("foreground-cut");
        std::fs::create_dir_all(&cut).unwrap();
        std::fs::write(cut.join("lose-answer.verb"), b"process.start").unwrap();
        script["runner_hold_cuts"] = json!(cut);
        // An unknown start ends the first turn before another model step.
        script["steps"].as_array_mut().unwrap().remove(1);
    }
    script["steps"][0]["output"]["tool"]["arguments"]["yield_time_ms"] = json!(3000);
    let command = script["steps"][0]["output"]["tool"]["arguments"]["command"].as_str().unwrap().replace("read line", &format!(
        "n=0; while [ ! -f '{}' ]; do [ \"$n\" -lt 30000 ] || exit 1; n=$((n + 1)); sleep 0.01; done; printf done > '{}'",
        fixture.root.join("foreground-release").display(), fixture.root.join("foreground-completed").display()));
    script["steps"][0]["output"]["tool"]["arguments"]["command"] = json!(command);
    let following = format!(
        "read old parent < '{}'; if kill -0 \"$old\" 2>/dev/null; then printf escaped > '{}'; fi; printf next > '{}'",
        fixture.root.join("local-job.marker").display(),
        fixture.root.join("effect-before-cleanup").display(),
        fixture.root.join("next-effect").display()
    );
    script["steps"].as_array_mut().unwrap().extend([
        json!({"output":{"tool":{"name":"process_session_start","arguments":{
            "command": following, "workdir":".","login":false,"tty":false,
            "keep_stdin_open":false,"yield_time_ms":1000,"max_output_bytes":8192}}},"finish":"tool_calls"}),
        json!({"output":{"text":ANSWER},"finish":"stop"}),
    ]);
    fixture.write_script(&script);
    let host = start_env(fixture, &[], env);
    let mut client = Client::connect(&host);
    // The daemon and model are admitted only on the following create/open.
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    assert!(matches!(
        client.request(
            3,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("foreground wake proof")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { .. })
    ));
    // The shell creates the marker before it writes its line.
    let deadline = std::time::Instant::now() + WAIT;
    while !std::fs::read_to_string(fixture.root.join("local-job.marker"))
        .is_ok_and(|marker| marker.ends_with('\n'))
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if matches!(mode, WakeMode::Settled) {
        while !fixture
            .root
            .join("foreground-cut/lose-answer.entered")
            .is_file()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "start answer was never held"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    (host, client, session)
}

fn pause(fixture: &Fixture, daemon: i32) -> Paused {
    let paused = Paused::new(daemon);
    if fixture.root.join("foreground-cut").is_dir() {
        std::fs::write(
            fixture.root.join("foreground-cut/lose-answer.release"),
            b"release",
        )
        .unwrap();
    }
    paused
}

fn terminal(client: &mut Client, mode: WakeMode) {
    let mut terminal = false;
    let mut idle = false;
    let mut recovery_failed = matches!(mode, WakeMode::Live);
    loop {
        match client.next() {
            HostFrame::Event {
                observation: plakous_host_protocol::SessionObservation::TurnLifecycle(value),
                ..
            } => terminal |= value.phase != plakous_host_protocol::TurnPhase::Admitted,
            HostFrame::Event {
                observation: plakous_host_protocol::SessionObservation::SetBusy(busy),
                ..
            } => idle = !busy,
            HostFrame::Event {
                observation:
                    plakous_host_protocol::SessionObservation::Status {
                        detail:
                            Some(plakous_host_protocol::status::HostStatusDetail::RecoveryIncomplete),
                        ..
                    },
                ..
            } => recovery_failed = true,
            _ => {}
        }
        // A live eager reopen may still fold this outcome on reconnect. Pin
        // its failed read while paused, rather than racing the terminal's idle.
        if terminal && idle && recovery_failed {
            return;
        }
    }
}

fn following_effect(fixture: &Fixture, client: &mut Client, session: &str, delays: &[u64]) {
    let node = client
        .rows(40)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
        .node;
    let attachment = client.attach(41, &node);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    client.read_deadline = Some(deadline);
    for (attempt, &delay) in delays.iter().enumerate() {
        std::thread::sleep(std::time::Duration::from_secs(delay));
        assert!(
            std::time::Instant::now() < deadline,
            "runner recovery exceeded its deadline"
        );
        let id = 42 + u64::try_from(attempt).unwrap();
        match client.try_prompt(id, attachment, &format!("following effect {id}"), ANSWER) {
            Ok(()) => {
                assert!(
                    fixture.root.join("next-effect").exists(),
                    "the prompt answered without its effect"
                );
                break;
            }
            Err(reason) => {
                assert!(
                    ![
                        "lease is held",
                        "branch is held",
                        "could not be confirmed gone",
                        "still reconciling",
                        "newer owner",
                        "StaleOwner"
                    ]
                    .iter()
                    .any(|text| reason.contains(text)),
                    "a prompt was refused for ownership: {reason}"
                );
                assert!(
                    reason.contains("runner did not reconnect")
                        || (reason.contains("runner placement") && reason.contains("not ready")),
                    "unexpected prompt refusal: {reason}"
                );
                assert!(
                    attempt + 1 < delays.len(),
                    "runner recovery exceeded its refusal budget: {reason}"
                );
            }
        }
    }
    client.read_deadline = None;
    assert!(fixture.root.join("next-effect").exists());
    assert!(
        !fixture.root.join("effect-before-cleanup").exists(),
        "a new effect passed unfinished foreground cleanup"
    );
}

fn no_job_terminals(fixture: &Fixture) {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let connection = rusqlite::Connection::open(paths.runtime_database()).unwrap();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM line_job_terminals", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0, "foreground is never a durable job");
}

#[test]
fn runner_wake_settled_foreground() {
    settled_foreground(std::time::Duration::ZERO);
}

#[test]
fn runner_wake_settled_foreground_after_delayed_reconnect() {
    settled_foreground(std::time::Duration::from_millis(2500));
}

#[test]
fn runner_reopen_waits_for_its_readoption() {
    let Some(fixture) = fixture() else { return };
    let hold = fixture.root.join("readoption-hold");
    std::fs::create_dir(&hold).unwrap();
    let (mut host, mut client, session) = begin_env(
        &fixture,
        WakeMode::Settled,
        &[(
            "PLAK_TEST_HOST_READOPT_CUT",
            format!("hold:{}", hold.display()),
        )],
    );
    let (pid, daemon) = job(&fixture);
    let mut paused = pause(&fixture, daemon);
    terminal(&mut client, WakeMode::Settled);
    std::thread::sleep(std::time::Duration::from_millis(2500));
    paused.resume();
    let deadline = std::time::Instant::now() + WAIT;
    while !hold.join("entered").is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "readoption never held the lease"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let node = client
        .rows(40)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
        .node;
    let attachment = client.attach(41, &node);
    let answered = std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while !hold.join("waiting").is_file() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::fs::write(hold.join("release"), b"release").unwrap();
        });
        client.try_prompt(42, attachment, "following effect", ANSWER)
    });
    assert!(
        answered.is_ok(),
        "a prompt racing this host's readoption was rejected: {:?}",
        host.lines.try_iter().collect::<Vec<_>>()
    );
    assert!(
        hold.join("waiting").is_file(),
        "the prompt never waited on readoption"
    );
    assert!(fixture.root.join("next-effect").is_file());
    assert!(!fixture.root.join("effect-before-cleanup").is_file());
    stopped(pid);
    job(&fixture);
    stop(&mut host);
}

fn settled_foreground(resume_delay: std::time::Duration) {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session) = begin(&fixture, WakeMode::Settled);
    let (pid, daemon) = job(&fixture);
    let mut paused = pause(&fixture, daemon);
    terminal(&mut client, WakeMode::Settled);
    assert!(alive(pid), "frozen awake time expired the owner lease");
    let transitions = effect_transitions(&fixture, &session);
    let operation = transitions
        .first()
        .expect("the effect was never dispatched")
        .0;
    let settled = effect_counts(&transitions, operation);
    assert_eq!(
        settled,
        EffectCounts {
            physical: 1,
            folds: 0
        },
        "the driver must settle the effect before failed eager recovery"
    );
    std::thread::sleep(resume_delay);
    paused.resume();
    stopped(pid);
    assert!(
        !fixture.root.join("foreground-completed").exists(),
        "the process exited naturally instead of keyed cleanup"
    );
    assert_eq!(
        effect_counts(&effect_transitions(&fixture, &session), operation),
        settled
    );
    no_job_terminals(&fixture);
    let delays: &[u64] = if resume_delay.is_zero() {
        &[0, 0]
    } else {
        &[0, 1, 2, 5, 15]
    };
    following_effect(&fixture, &mut client, &session, delays);
    assert_eq!(
        effect_counts(&effect_transitions(&fixture, &session), operation),
        EffectCounts {
            physical: 1,
            folds: 1
        },
        "reopen folds the same physical outcome exactly once"
    );
    job(&fixture);
    stop(&mut host);
}

#[test]
fn runner_wake_live_foreground() {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session) = begin(&fixture, WakeMode::Live);
    let (pid, daemon) = job(&fixture);
    let mut paused = pause(&fixture, daemon);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(alive(pid));
    paused.resume();
    std::fs::write(fixture.root.join("foreground-release"), b"release").unwrap();
    terminal(&mut client, WakeMode::Live);
    assert_eq!(
        std::fs::read(fixture.root.join("foreground-completed")).unwrap(),
        b"done"
    );
    assert_eq!(unknown_effects(&fixture, &session), 0);
    no_job_terminals(&fixture);
    job(&fixture);
    stop(&mut host);
}

#[test]
fn runner_lease_lapse_reports_foreground() {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session) = begin(&fixture, WakeMode::Settled);
    let (pid, daemon) = job(&fixture);
    let mut paused = pause(&fixture, daemon);
    terminal(&mut client, WakeMode::Settled);
    let unknown = unknown_effects(&fixture, &session);
    clock(&fixture, 301_000);
    paused.resume();
    stopped(pid);
    no_job_terminals(&fixture);
    assert_eq!(unknown_effects(&fixture, &session), unknown);
    job(&fixture);
    stop(&mut host);
}
