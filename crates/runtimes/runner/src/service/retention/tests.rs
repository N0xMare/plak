#![allow(clippy::unwrap_used, reason = "isolated retention authority fixtures")]

use super::*;
use crate::state::Tables;
use crate::{FileOwnerFloors, OwnerFloors};
use plakous_model::CancellationToken;
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    JoinedProcessCommand, JoinedProcessInputDisposition, JoinedProcessStartRequest,
    JoinedProcessTerminalMode,
};
use std::sync::Arc;

fn first() -> LineOwner {
    LineOwner {
        line: "line".into(),
        generation: 1,
        branch: "branch".into(),
        fence: 1,
    }
}

#[test]
fn review_c47_orphan_does_not_strip_retention_authority() {
    let root = tempfile::tempdir().unwrap();
    let mut tables = Tables::with_floors(Arc::new(
        FileOwnerFloors::open(root.path().join("owners.json")).unwrap(),
    ));
    let from = first();
    tables.transfer("home", "same", "native".into(), None, from.clone());
    let to = LineOwner {
        fence: 2,
        ..from.clone()
    };
    tables.transfer_retained("home", "same", "retain".into(), &from, to.clone());
    tables.orphan("home", "same");
    assert!(tables.retained_by("home", "same", &to));
    assert!(!tables.held_by("home", "same", &to));
    assert_eq!(tables.held_lines("home", "same"), []);
}

#[test]
fn retention_transfer_persists_native_ban_and_lost_ack_reconciles_only_own_tuple() {
    let root = tempfile::tempdir().unwrap();
    let floors = Arc::new(FileOwnerFloors::open(root.path().join("owners.json")).unwrap());
    let mut tables = Tables::with_floors(floors.clone());
    let from = first();
    assert!(matches!(
        tables.transfer("home", "old", "native".into(), None, from.clone()),
        RunnerFrame::TransferAccepted { .. }
    ));
    let to = LineOwner {
        fence: 2,
        ..from.clone()
    };
    assert!(matches!(
        tables.transfer_retained("home", "new", "retain".into(), &from, to.clone()),
        RunnerFrame::TransferAccepted { .. }
    ));
    assert!(floors.load()[0].1.retention_only);
    let reopened = FileOwnerFloors::open(root.path().join("owners.json")).unwrap();
    let mut restarted = Tables::with_floors(Arc::new(reopened));
    assert!(matches!(
        restarted.transfer("home", "new", "downgrade".into(), Some(&to), to.clone()),
        RunnerFrame::TransferRefused { .. }
    ));
    assert!(matches!(
        restarted.transfer_retained("home", "new", "lost-ack".into(), &from, to.clone()),
        RunnerFrame::TransferAccepted { .. }
    ));
    assert!(!restarted.held_by("home", "new", &to));
    assert!(restarted.retained_by("home", "new", &to));
    assert!(matches!(
        restarted.transfer_retained("home", "foreign", "steal".into(), &from, to.clone()),
        RunnerFrame::StaleOwner { .. }
    ));
    assert!(restarted.owner("other-home", "line").is_none());
}

#[test]
fn retention_recovery_never_dispatches_an_unseen_job_control_or_exposes_it_natively() {
    let root = tempfile::tempdir().unwrap();
    let mut tables = Tables::with_floors(Arc::new(
        FileOwnerFloors::open(root.path().join("owners.json")).unwrap(),
    ));
    let from = first();
    tables.transfer("home", "old", "native".into(), None, from.clone());
    let to = LineOwner {
        fence: 2,
        ..from.clone()
    };
    tables.transfer_retained("home", "new", "retain".into(), &from, to.clone());
    let mut request = Request {
        request_id: "request".into(),
        owner: Some(to),
        class: RequestClass::OwnerControl {
            control_id: "input".into(),
        },
        deadline_ms: None,
        digest: "digest".into(),
        replay: true,
        age_ms: Some(1),
        operation: Operation {
            verb: RunnerVerb::ProcessWrite.as_str().into(),
            payload: json!({}),
        },
    };
    assert!(matches!(
        tables.admit_retained("home", "new", &request, Instant::now()),
        Admission::Answer(RunnerFrame::NotSeen { .. })
    ));
    request.replay = false;
    assert!(matches!(
        tables.admit_retained("home", "new", &request, Instant::now()),
        Admission::Answer(RunnerFrame::NotSeen { .. })
    ));
    assert!(matches!(
        tables.admit("home", "new", &request, Instant::now()),
        Admission::Answer(RunnerFrame::Refused { .. })
    ));
}

#[tokio::test]
async fn consumed_unjoined_terminal_keeps_native_ack_and_retention_answers() {
    let root = tempfile::tempdir().unwrap();
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let sessions = environment.joined_process_sessions().unwrap();
    let control = OperationContext::new(CancellationToken::new());
    let process = sessions
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal("/bin/cat", Vec::<String>::new()).unwrap(),
                ".",
                Vec::new(),
                true,
                JoinedProcessTerminalMode::Pipes,
            )
            .unwrap(),
            control.clone(),
        )
        .unwrap();
    sessions.promote_without_monitor_for_test(process).unwrap();
    let mut changes = process_session_changes(&sessions, process).unwrap();
    sessions
        .write_stdin(process, b"retained\n", JoinedProcessInputDisposition::Close)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while sessions.joined_terminal_status(process).is_none() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let status = JoinedProcessStatus::Exited(Some(0));
    assert_eq!(sessions.joined_terminal_status(process), Some(status));

    let assert_terminal_snapshot = || {
        let Answer::Jobs { jobs, .. } = snapshot(&sessions, 0).unwrap() else {
            panic!("expected a retention snapshot");
        };
        let [job] = jobs.as_slice() else {
            panic!("expected the unretired terminal, got {jobs:?}");
        };
        assert!(job.terminal);
        assert_eq!(job.status, wire::Status::Exited { code: Some(0) });
        job.session.clone()
    };
    let job = assert_terminal_snapshot();
    for _ in 0..2 {
        serve_sessions(
            &environment,
            &sessions,
            "fixture",
            RunnerVerb::JobsAck.as_str(),
            json!({ "numbers": [JoinedProcessSessions::job_number(process)] }),
            control.clone(),
            false,
        )
        .await
        .unwrap();
        assert!(!sessions.has_unreported_terminal_job());
        assert_eq!(assert_terminal_snapshot(), job);
        assert_eq!(session(&sessions, &job), Ok(process));
    }
    let output = read(&sessions, &job, 0, 1024, 0).await.unwrap();
    assert!(matches!(
        &output,
        Answer::Output {
            status: wire::ProcessStatus::Exited { code: Some(0) },
            ..
        }
    ));
    assert_eq!(read(&sessions, &job, 0, 1024, 0).await.unwrap(), output);
    assert_eq!(sessions.join(process).unwrap(), status);
}

#[test]
fn sequence_ignores_volatile_elapsed_ms_and_tracks_state_changes() {
    let mut first = wire::Job {
        session: id("00000000000000000000000000000001").unwrap(),
        number: 1,
        command: "sleep 60".into(),
        started_at_ms: 1_700_000_000_000,
        attribution: "agent".into(),
        elapsed_ms: 10,
        status: wire::Status::Running {},
        terminal: false,
        output: String::new(),
        owner_generation: 1,
    };
    let mut later = first.clone();
    later.elapsed_ms = 5_000;
    assert_eq!(
        sequence(std::slice::from_ref(&first)).unwrap(),
        sequence(&[later]).unwrap()
    );

    first.output = "hello\n".into();
    assert_ne!(
        sequence(&[first.clone()]).unwrap(),
        sequence(&[wire::Job {
            output: String::new(),
            ..first.clone()
        }])
        .unwrap()
    );

    let exited = wire::Job {
        status: wire::Status::Exited { code: Some(0) },
        terminal: true,
        ..first.clone()
    };
    assert_ne!(sequence(&[first]).unwrap(), sequence(&[exited]).unwrap());
}

#[tokio::test]
async fn jobs_watch_waits_while_running_job_only_advances_elapsed_ms() {
    let root = tempfile::tempdir().unwrap();
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000002".parse().unwrap()),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let sessions = environment.joined_process_sessions().unwrap();
    let control = OperationContext::new(CancellationToken::new());
    let process = sessions
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal("/bin/sleep", vec!["5".to_owned()]).unwrap(),
                ".",
                Vec::new(),
                true,
                JoinedProcessTerminalMode::Pipes,
            )
            .unwrap(),
            control,
        )
        .unwrap();
    sessions.promote_without_monitor_for_test(process).unwrap();

    let Answer::Jobs {
        sequence: initial_seq,
        jobs: initial_jobs,
        ..
    } = snapshot(&sessions, 0).unwrap()
    else {
        panic!("expected initial jobs snapshot");
    };
    assert_eq!(initial_jobs.len(), 1);

    let started = Instant::now();
    let watch_owner = owner(&first()).unwrap();
    let Answer::Jobs {
        sequence: watched_seq,
        jobs: watched_jobs,
        ..
    } = observe(
        &sessions,
        &wire::Request::JobsWatch {
            owner: watch_owner,
            sequence: initial_seq,
            wait_ms: 120,
        },
    )
    .await
    .unwrap()
    else {
        panic!("expected jobs watch answer");
    };
    let waited = started.elapsed();
    assert_eq!(watched_seq, initial_seq);
    assert!(
        waited >= Duration::from_millis(100),
        "JobsWatch returned early after {waited:?} despite no job state change"
    );
    assert!(
        watched_jobs[0].elapsed_ms >= initial_jobs[0].elapsed_ms,
        "elapsed_ms should still be reported fresh"
    );

    let _ = sessions.terminate(process);
}
