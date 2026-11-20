use super::*;
use plakous_runner_protocol::retention::{Job, PAGE_SIZE};
use plakous_tools::{JobStatus, JoinedProcessOutputCursor};

async fn populate(sessions: &RunnerSessions, count: u32) -> plakous_tools::JoinedProcessSessionId {
    let mut first = None;
    for number in 1..=count {
        let id = sessions
            .start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::literal(
                        "/bin/sh",
                        ["-c".into(), format!("printf terminal-{number}")],
                    )
                    .unwrap(),
                    ".",
                    Vec::new(),
                    false,
                    JoinedProcessTerminalMode::Pipes,
                )
                .unwrap(),
                OperationContext::new(plakous_model::CancellationToken::new()).with_class(
                    OperationClass::EffectCall {
                        call_id: format!("00000000-0000-0000-0000-{number:012x}")
                            .parse()
                            .unwrap(),
                        attempt_id: "00000000-0000-0000-0001-000000000001".parse().unwrap(),
                    },
                ),
            )
            .await
            .unwrap();
        sessions.promote(id).await.unwrap();
        first.get_or_insert(id);
        wait_for_terminal_job(sessions, number).await;
    }
    first.unwrap()
}

async fn wait_for_terminal_job(sessions: &RunnerSessions, number: u32) {
    // A page of retained terminals need not be a page of live processes.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let jobs = sessions.jobs().await.unwrap();
            if jobs
                .iter()
                .any(|job| job.number == number && matches!(job.status, JobStatus::Exited(Some(0))))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn all_terminals(service: &RunnerService, owner: &LineOwner) -> Vec<Job> {
    let mut collected = Vec::new();
    let mut after = 0;
    loop {
        let RunnerFrame::JobTerminals {
            owner: actual,
            jobs,
            next,
            ..
        } = service
            .job_terminals("home", "before", "page".into(), owner, after)
            .await
        else {
            panic!("terminal page")
        };
        assert_eq!(&actual, owner);
        assert!(jobs.iter().all(|job| job.terminal));
        collected.extend(jobs);
        let Some(next) = next else {
            break;
        };
        after = next;
    }
    collected
}

async fn add_live_job(sessions: &RunnerSessions, number: u32) {
    let id = sessions
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal("/bin/cat", Vec::<String>::new()).unwrap(),
                ".",
                Vec::new(),
                true,
                JoinedProcessTerminalMode::Pipes,
            )
            .unwrap(),
            OperationContext::new(plakous_model::CancellationToken::new()).with_class(
                OperationClass::EffectCall {
                    call_id: format!("00000000-0000-0000-0000-{number:012x}")
                        .parse()
                        .unwrap(),
                    attempt_id: "00000000-0000-0000-0001-000000000001".parse().unwrap(),
                },
            ),
        )
        .await
        .unwrap();
    sessions.promote(id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_terminals_pages_are_complete_original_and_leave_jobs_and_output_unconsumed() {
    let root = tempfile::tempdir().unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let (service, link) = link_with_lease(root.path(), scope, Duration::from_secs(300)).await;
    let backend = link.backend();
    let environment = runner_environment(
        scope,
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend.clone(),
    )
    .unwrap();
    let owner = plakous_tools::LineOwner::new(
        "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        1,
    );
    let sessions = RunnerSessions::new(&backend, &environment, owner);
    let count = u32::try_from(PAGE_SIZE + 1).unwrap();
    let first = populate(&sessions, count).await;
    add_live_job(&sessions, count + 1).await;
    let before = sessions.jobs().await.unwrap();
    assert!(before.iter().any(|job| job.status == JobStatus::Running));
    let native = LineOwner {
        line: owner.line().to_string(),
        generation: 1,
        branch: owner.branch().to_string(),
        fence: 1,
    };
    let wrong = LineOwner {
        fence: 2,
        ..native.clone()
    };
    assert!(matches!(
        service
            .job_terminals("home", "before", "wrong".into(), &wrong, 0)
            .await,
        RunnerFrame::Refused { .. }
    ));
    assert!(matches!(
        service
            .job_terminals("other", "before", "other".into(), &native, 0)
            .await,
        RunnerFrame::Refused { .. }
    ));
    let collected = all_terminals(&service, &native).await;
    assert_eq!(
        collected.iter().map(|job| job.number).collect::<Vec<_>>(),
        (1..=count).collect::<Vec<_>>()
    );
    assert_eq!(
        sessions.jobs().await.unwrap().len(),
        before.len(),
        "proof reading is not jobs.ack"
    );
    let output = sessions
        .poll_from(
            first,
            JoinedProcessOutputCursor::new(0),
            4096,
            64,
            Duration::ZERO,
            OperationContext::new(plakous_model::CancellationToken::new()),
        )
        .await
        .unwrap();
    let bytes = output
        .records()
        .iter()
        .flat_map(|record| record.bytes().iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(String::from_utf8(bytes).unwrap(), "terminal-1");
    assert_eq!(sessions.jobs().await.unwrap().len(), before.len());
    assert_eq!(
        lock(&service.inner.tables)
            .owner("home", &native.line)
            .unwrap()
            .0,
        native
    );
    sessions
        .stop_jobs(plakous_tools::JobStopReason::User, Duration::ZERO)
        .await
        .unwrap();
}
