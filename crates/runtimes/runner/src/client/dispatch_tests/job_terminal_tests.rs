use super::*;
use plakous_runner_protocol::retention::{Id, Job, PAGE_SIZE, Status};

fn owner() -> LineOwner {
    LineOwner {
        line: "line".into(),
        generation: 1,
        branch: "branch".into(),
        fence: 2,
    }
}

fn job(number: u32) -> Job {
    Job {
        session: Id::try_from(format!("{number:032x}")).unwrap(),
        number,
        command: "completed".into(),
        started_at_ms: 1,
        attribution: "agent".into(),
        elapsed_ms: 1,
        status: Status::Exited { code: Some(0) },
        terminal: true,
        output: format!("output-{number}"),
        owner_generation: 1,
    }
}

fn journal(root: &std::path::Path) -> Arc<ClosingJournal> {
    Arc::new(ClosingJournal {
        files: FileCallJournal::open(root).unwrap(),
        closer: CancellationToken::new(),
        claims: AtomicU64::new(0),
    })
}

#[tokio::test]
async fn job_terminals_join_all_pages_without_claiming_or_acknowledging_effects() {
    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path());
    let (client, mut frames) = connected(&journal);
    let reading = client.clone();
    let task = tokio::spawn(async move { reading.job_terminals(&owner()).await });
    let HostFrame::JobTerminals {
        request_id,
        owner: read_owner,
        after,
    } = frames.recv().await.unwrap()
    else {
        panic!("terminal read")
    };
    assert_eq!(after, 0);
    assert_eq!(read_owner, owner());
    let boundary = u32::try_from(PAGE_SIZE).unwrap();
    client.inner.receive(RunnerFrame::JobTerminals {
        request_id,
        owner: owner(),
        jobs: (1..=boundary).map(job).collect(),
        next: Some(boundary),
    });
    let HostFrame::JobTerminals {
        request_id, after, ..
    } = frames.recv().await.unwrap()
    else {
        panic!("next terminal page")
    };
    assert_eq!(after, boundary);
    client.inner.receive(RunnerFrame::JobTerminals {
        request_id,
        owner: owner(),
        jobs: vec![job(boundary + 1)],
        next: None,
    });
    assert_eq!(
        task.await.unwrap().unwrap(),
        (1..=boundary + 1).map(job).collect::<Vec<_>>()
    );
    assert!(frames.try_recv().is_err());
    assert!(lock(&client.inner.state).questions.is_empty());
    assert_eq!(journal.claims.load(Ordering::Relaxed), 0);
    assert_eq!(journal.pending().unwrap(), []);
}

#[tokio::test]
async fn job_terminals_refuse_owner_drift_partial_pages_and_nonoriginal_outcomes() {
    for fault in ["owner", "partial", "terminal", "unknown", "interrupted"] {
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path());
        let (client, mut frames) = connected(&journal);
        let reading = client.clone();
        let task = tokio::spawn(async move { reading.job_terminals(&owner()).await });
        let HostFrame::JobTerminals { request_id, .. } = frames.recv().await.unwrap() else {
            panic!("terminal read")
        };
        let mut reply_owner = owner();
        let mut row = job(1);
        if fault == "owner" {
            reply_owner.fence += 1;
        }
        if fault == "terminal" {
            row.terminal = false;
        }
        if fault == "unknown" {
            row.status = Status::OutcomeUnknown {};
        }
        if fault == "interrupted" {
            lock(&client.inner.state).questions.remove(&request_id);
        } else {
            client.inner.receive(RunnerFrame::JobTerminals {
                request_id,
                owner: reply_owner,
                jobs: vec![row],
                next: (fault == "partial").then_some(1),
            });
        }
        assert!(task.await.unwrap().is_err(), "{fault}");
        assert!(frames.try_recv().is_err());
        assert!(lock(&client.inner.state).questions.is_empty());
        assert_eq!(journal.claims.load(Ordering::Relaxed), 0);
    }
}
