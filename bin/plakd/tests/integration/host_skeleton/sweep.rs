//! Control-socket adapter for indexed host/daemon crash sweeps.

use std::collections::BTreeSet;
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
                    .strip_prefix("plakd host: serving on ")
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
            (records, _) = drive(
                &host,
                session,
                true,
                shape == "host_handoff_keeps_jobs",
                answer,
                &[],
            );
        } else {
            // Serving the directory does not admit a runner. The traced pilot
            // must finish readoption and any recorded stop before shutdown;
            // otherwise runtime cancellation can interrupt its trace writer.
            runner::wait_local_runner_ready(&host);
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
) -> (Vec<String>, BTreeSet<String>) {
    let mut client = Client::connect(host);
    let row = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap();
    let mut records = Vec::new();
    let mut unknown = BTreeSet::new();
    client.retain_observations();
    let attachment = client.attach(2, &row.node);
    for frame in client.retained.take().unwrap_or_default() {
        if let HostFrame::Event { observation, .. } = frame {
            unknown.extend(outcome_unknown_jobs(&observation));
            project(&observation, &mut records);
        }
    }
    if prompt {
        assert_eq!(
            client.request(
                3,
                Op::Intent {
                    client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                    attachment,
                    intent: SessionIntent::Prompt(Draft::from("crash-sweep-original-prompt")),
                    blobs: Vec::new(),
                }
            ),
            Outcome::Ok(Answer::Accepted { duplicate: false })
        );
    }
    let mut answered = answer.is_empty();
    let mut settled = answer.is_empty();
    let mut idle = false;
    loop {
        if let HostFrame::Event { observation, .. } = client.next() {
            answered |= format!("{observation:?}").contains(answer);
            settled |= matches!(&observation, plakous_host_protocol::SessionObservation::TurnLifecycle(lifecycle) if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted);
            idle |= matches!(
                observation,
                plakous_host_protocol::SessionObservation::SetBusy(false)
            );
            unknown.extend(outcome_unknown_jobs(&observation));
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
    (records, unknown)
}

/// The jobs `observation` reports as `OutcomeUnknown`. The live terminal
/// event reaches only an attachment admitted before reconciliation ends; a
/// later attachment sees the outcome in its attach-time process state.
fn outcome_unknown_jobs(
    observation: &plakous_host_protocol::SessionObservation<PlakExtension>,
) -> Vec<String> {
    use plakous_host_protocol::SessionObservation;
    use plakous_host_protocol::views::ProcessStatus;
    match observation {
        SessionObservation::JobTerminal { job_id, status, .. } if status == "outcome_unknown" => {
            vec![job_id.clone()]
        }
        SessionObservation::Processes { views, .. } => views
            .iter()
            .filter(|view| view.status == ProcessStatus::OutcomeUnknown)
            .filter_map(|view| view.id.rsplit_once(':').map(|(_, job)| job.to_owned()))
            .collect(),
        _ => Vec::new(),
    }
}

fn project(
    observation: &plakous_host_protocol::SessionObservation<PlakExtension>,
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

/// Restarts the host over `fixture` and drives `session` to rest, returning
/// the jobs it reported as `OutcomeUnknown`.
pub(crate) fn recover(
    fixture: &Fixture,
    session: &str,
    answer: &str,
    unknown_jobs: &[u32],
) -> BTreeSet<String> {
    use plakous_hosted::runtime::RootStanding;
    let store = plakd::store::session_records::SqliteSessionStore::new(
        plakd::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let owes = matches!(
        plakous_hosted::runtime::root_standing(&store, session).unwrap(),
        RootStanding::Continuing { handoff: true, .. }
            | RootStanding::Continuing { held: false, .. }
    );
    let mut host = start(fixture);
    let (_, unknown) = drive(
        &host,
        session,
        false,
        false,
        if owes { answer } else { "" },
        unknown_jobs,
    );
    let status = finish(&mut host);
    assert!(status.success());
    unknown
}

pub(super) fn record_stop(fixture: &Fixture, session: &str) {
    use plakd::store::session_records::SessionRecordStore as _;
    use plakous_hosted::sessions::projector::{RootInterrupt, RootLossIntent, RootStopAnchor};
    let store = plakd::store::session_records::SqliteSessionStore::new(
        plakd::home::paths::PlakPaths::explicit(fixture.home()),
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

#[test]
fn attach_time_process_state_reports_lost_jobs() {
    use plakous_host_protocol::SessionObservation;
    use plakous_host_protocol::views::{ProcessStatus, ProcessView};
    let view = |job: u32, status| ProcessView {
        id: format!("session:branch:p{job}"),
        command: "sleep 300".into(),
        owner: "root".into(),
        started_at: 0,
        elapsed_ms: 0,
        status,
        output: String::new(),
    };
    let processes = SessionObservation::<PlakExtension>::Processes {
        views: vec![
            view(1, ProcessStatus::OutcomeUnknown),
            view(2, ProcessStatus::Running),
            view(3, ProcessStatus::Exited(Some(0))),
        ],
        running: 1,
    };
    assert_eq!(outcome_unknown_jobs(&processes), ["p1"]);
    let terminal = |status: &str| SessionObservation::<PlakExtension>::JobTerminal {
        job_id: "p4".into(),
        owner: "root".into(),
        status: status.into(),
        exit_code: None,
        stop_reason: None,
        elapsed_ms: 0,
        output: String::new(),
    };
    assert_eq!(outcome_unknown_jobs(&terminal("outcome_unknown")), ["p4"]);
    assert_eq!(
        outcome_unknown_jobs(&terminal("exited")),
        Vec::<String>::new()
    );
}
