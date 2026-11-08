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

fn begin(fixture: &Fixture, window: u64) -> (Host, Client, String) {
    clock(fixture, 0);
    let mut script = job_script(fixture, false);
    script["runner_window_ms"] = json!(window);
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
    let host = start(fixture);
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
    let deadline = std::time::Instant::now() + WAIT;
    while !fixture.root.join("local-job.marker").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    (host, client, session)
}

fn terminal(client: &mut Client) {
    let mut terminal = false;
    let mut idle = false;
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
            _ => {}
        }
        if terminal && idle {
            return;
        }
    }
}

fn following_effect(fixture: &Fixture, client: &mut Client, session: &str) {
    let node = client
        .rows(40)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
        .node;
    let attachment = client.attach(41, &node);
    for id in [42, 43] {
        client.prompt(id, attachment, "following effect", ANSWER);
        if fixture.root.join("next-effect").exists() {
            break;
        }
    }
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
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session) = begin(&fixture, 1200);
    let (pid, daemon) = job(&fixture);
    let mut paused = Paused::new(daemon);
    terminal(&mut client);
    assert!(alive(pid), "frozen awake time expired the owner lease");
    let settled = effect_settlements(&fixture, &session, false);
    assert!(
        settled > 0,
        "the driver did not settle its unavailable effect"
    );
    paused.resume();
    stopped(pid);
    assert!(
        !fixture.root.join("foreground-completed").exists(),
        "the process exited naturally instead of keyed cleanup"
    );
    assert_eq!(effect_settlements(&fixture, &session, false), settled);
    no_job_terminals(&fixture);
    following_effect(&fixture, &mut client, &session);
    job(&fixture);
    stop(&mut host);
}

#[test]
fn runner_wake_live_foreground() {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session) = begin(&fixture, 10000);
    let (pid, daemon) = job(&fixture);
    let mut paused = Paused::new(daemon);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(alive(pid));
    paused.resume();
    std::fs::write(fixture.root.join("foreground-release"), b"release").unwrap();
    terminal(&mut client);
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
    let (mut host, mut client, session) = begin(&fixture, 1200);
    let (pid, daemon) = job(&fixture);
    let mut paused = Paused::new(daemon);
    terminal(&mut client);
    let unknown = unknown_effects(&fixture, &session);
    clock(&fixture, 301_000);
    paused.resume();
    stopped(pid);
    no_job_terminals(&fixture);
    assert_eq!(unknown_effects(&fixture, &session), unknown);
    job(&fixture);
    stop(&mut host);
}
