//! Control-socket adapter for indexed host/daemon crash sweeps.

use std::time::{Duration, Instant};

use super::*;

pub(crate) fn run(
    fixture: &Fixture,
    session: &str,
    shape: &str,
    answer: &str,
    env: &[(&str, String)],
) -> AgentOutcome {
    if matches!(shape, "host_crash_mid_readopt" | "host_crash_mid_stop") {
        let mut host = start(fixture);
        drive(&host, session, true, false, answer, &[]);
        kill(&mut host);
        if shape == "host_crash_mid_stop" {
            record_stop(fixture, session);
        }
    }
    let (mut child, lines) = spawn_env(fixture, &[], env);
    let mut stderr = Vec::new();
    let socket = loop {
        match lines.recv_timeout(WAIT) {
            Ok(line) => {
                let socket = line
                    .strip_prefix("plak host: serving on ")
                    .and_then(|rest| rest.split(" (host ").next())
                    .map(PathBuf::from);
                stderr.push(line);
                if let Some(socket) = socket {
                    break socket;
                }
            }
            Err(_) => {
                return AgentOutcome {
                    status: child.wait().unwrap(),
                    lines: Vec::new(),
                    stderr: stderr.join("\n"),
                };
            }
        }
    };
    let mut host = Host {
        child: Some(child),
        socket,
        lines,
    };
    let prompts = !matches!(shape, "host_crash_mid_readopt" | "host_crash_mid_stop");
    let mut records = Vec::new();
    let driven = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if prompts {
            records = drive(
                &host,
                session,
                true,
                shape == "host_handoff_keeps_jobs",
                answer,
                &[],
            );
        }
    }));
    if let Err(error) = driven {
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.child.as_mut().unwrap().try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                std::panic::resume_unwind(error);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let status = finish(&mut host);
    stderr.extend(host.lines.try_iter());
    AgentOutcome {
        status,
        lines: records,
        stderr: stderr.join("\n"),
    }
}

fn drive(
    host: &Host,
    session: &str,
    prompt: bool,
    busy: bool,
    answer: &str,
    unknown_jobs: &[u32],
) -> Vec<String> {
    let mut client = Client::connect(host);
    let row = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap();
    let mut records = Vec::new();
    client.send(&ClientFrame::Request {
        id: 2,
        op: Op::Attach { node: row.node },
    });
    let attachment = loop {
        match client.next() {
            HostFrame::Response {
                id: 2,
                outcome: Outcome::Ok(Answer::Attached { attachment }),
            } => break attachment,
            HostFrame::Event { observation, .. } => project(&observation, &mut records),
            other => assert!(
                !matches!(other, HostFrame::Response { id: 2, .. }),
                "attach refused: {other:?}"
            ),
        }
    };
    if prompt {
        assert_eq!(
            client.request(
                3,
                Op::Intent {
                    attachment,
                    intent: SessionIntent::Prompt(Draft::from("crash-sweep-original-prompt")),
                    blobs: Vec::new(),
                }
            ),
            Outcome::Ok(Answer::Accepted)
        );
    }
    let mut answered = answer.is_empty();
    let mut settled = answer.is_empty();
    let mut idle = false;
    let mut unknown = records
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row["kind"] == "job_terminal" && row["status"] == "outcome_unknown")
        .filter_map(|row| row["job_id"].as_str().map(str::to_owned))
        .collect::<std::collections::BTreeSet<_>>();
    loop {
        if let HostFrame::Event { observation, .. } = client.next() {
            answered |= format!("{observation:?}").contains(answer);
            settled |= matches!(&observation, plakous_host_protocol::SessionObservation::TurnLifecycle(lifecycle) if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted);
            idle |= matches!(
                observation,
                plakous_host_protocol::SessionObservation::SetBusy(false)
            );
            if let plakous_host_protocol::SessionObservation::JobTerminal { job_id, status, .. } =
                &observation
                && status == "outcome_unknown"
            {
                unknown.insert(job_id.clone());
            }
            project(&observation, &mut records);
            if answered
                && (busy
                    || (settled
                        && idle
                        && unknown_jobs
                            .iter()
                            .all(|job| unknown.contains(&format!("p{job}")))))
            {
                break;
            }
        }
    }
    records
}

fn project(
    observation: &plakous_host_protocol::SessionObservation<Core>,
    records: &mut Vec<String>,
) {
    if let Some(line) = plakous_host_protocol::jsonl::project_host_event(
        observation,
        plakous_host_protocol::envelope::JsonlCorrelation::default(),
    ) {
        records.push(line);
    }
}

fn finish(host: &mut Host) -> std::process::ExitStatus {
    let child = host.child.as_mut().unwrap();
    if child.try_wait().unwrap().is_none() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id().cast_signed()),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            host.child = None;
            return status;
        }
        assert!(Instant::now() < deadline, "host sweep did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(crate) fn recover(
    fixture: &Fixture,
    session: &str,
    answer: &str,
    unknown_jobs: &[u32],
) -> AgentOutcome {
    use plakous_hosted::runtime::RootStanding;
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let owes = matches!(
        plakous_hosted::runtime::root_standing(&store, session).unwrap(),
        RootStanding::Continuing { handoff: true, .. }
            | RootStanding::Continuing { held: false, .. }
    );
    let mut host = start(fixture);
    let records = drive(
        &host,
        session,
        false,
        false,
        if owes { answer } else { "" },
        unknown_jobs,
    );
    let status = finish(&mut host);
    assert!(status.success());
    AgentOutcome {
        status,
        lines: records,
        stderr: host.lines.try_iter().collect::<Vec<_>>().join("\n"),
    }
}

pub(super) fn record_stop(fixture: &Fixture, session: &str) {
    use plak::store::session_records::SessionRecordStore as _;
    use plakous_hosted::sessions::projector::{RootInterrupt, RootLossIntent, RootStopAnchor};
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    store
        .write_root_loss(
            session,
            RootLossIntent::Interrupt(RootInterrupt {
                anchor: RootStopAnchor::Command {
                    command_id: "plak:sweep:stop".into(),
                },
                cause: RootInterruptCause::Stopped,
            }),
        )
        .unwrap();
}
