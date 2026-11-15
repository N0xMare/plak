//! A cancelled transport is not a dispatch target, even before teardown drains it.

#![allow(clippy::unwrap_used, reason = "isolated dispatch and journal fixture")]

use super::*;
use crate::FileCallJournal;
use std::io;

mod job_terminal_tests;

struct ClosingJournal {
    files: FileCallJournal,
    closer: CancellationToken,
    claims: AtomicU64,
}

impl CallJournal for ClosingJournal {
    fn claim(&self, entry: &JournalEntry) -> io::Result<()> {
        self.files.claim(entry)?;
        self.claims.fetch_add(1, Ordering::Relaxed);
        self.closer.cancel();
        Ok(())
    }

    fn settle(&self, id: &str) -> io::Result<()> {
        self.files.settle(id)
    }

    fn pending(&self) -> io::Result<Vec<JournalEntry>> {
        self.files.pending()
    }
}

fn connected(journal: &Arc<ClosingJournal>) -> (RunnerClient, mpsc::UnboundedReceiver<HostFrame>) {
    let (sender, frames) = mpsc::unbounded_channel();
    let client = RunnerClient {
        inner: Arc::new(Inner {
            foreground: foreground::Calls::default(),
            connected: tokio::sync::Notify::new(),
            foreground_reporter: Mutex::default(),
            bind: Bind {
                host_id: "home".into(),
                host_instance: "host-instance".into(),
                version: VersionTriple::current("fixture", "test"),
                inspect: false,
            },
            windows: ClientWindows::default(),
            home_local: false,
            qualification: false,
            journal: Some(journal.clone()),
            left: Mutex::default(),
            ids: "client".into(),
            next: AtomicU64::new(0),
            state: Mutex::new(State {
                sender: Some(sender),
                connection: 1,
                closer: journal.closer.clone(),
                last_heard: Instant::now(),
                connected_at: Instant::now(),
                instance: "runner-instance".into(),
                pending: HashMap::new(),
                transfers: HashMap::new(),
                questions: HashMap::new(),
                witness_releases: HashMap::new(),
                refusal: None,
                streams: HashMap::new(),
                operator_streams: HashMap::new(),
            }),
            transferred: Mutex::default(),
        }),
    };
    (client, frames)
}

fn request() -> Request {
    Request {
        request_id: "client.r1".into(),
        class: RequestClass::OwnerControl {
            control_id: "control".into(),
        },
        owner: None,
        deadline_ms: None,
        digest: "digest".into(),
        replay: false,
        age_ms: None,
        operation: Operation {
            verb: RunnerVerb::MutateWorkspace.as_str().into(),
            payload: serde_json::json!({}),
        },
    }
}

#[tokio::test]
async fn dropped_owner_floor_query_releases_only_its_reply_slot_and_never_claims_a_key() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(ClosingJournal {
        files: FileCallJournal::open(root.path()).unwrap(),
        closer: CancellationToken::new(),
        claims: AtomicU64::new(0),
    });
    let (client, mut frames) = connected(&journal);
    let reading = client.clone();
    let first = tokio::spawn(async move { reading.owner_floors(&["line".into()]).await });
    let HostFrame::OwnerFloors {
        request_id: lost, ..
    } = frames.recv().await.unwrap()
    else {
        panic!("floor read")
    };
    let reading = client.clone();
    let second = tokio::spawn(async move { reading.owner_floors(&["other".into()]).await });
    let HostFrame::OwnerFloors {
        request_id: live, ..
    } = frames.recv().await.unwrap()
    else {
        panic!("second floor read")
    };
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(!lock(&client.inner.state).questions.contains_key(&lost));
    assert!(lock(&client.inner.state).questions.contains_key(&live));
    client.inner.receive(RunnerFrame::OwnerFloors {
        request_id: lost,
        floors: vec![],
    });
    client.inner.receive(RunnerFrame::OwnerFloors {
        request_id: live,
        floors: vec![plakous_runner_protocol::OwnerFloor {
            retained: Vec::new(),
            line: "other".into(),
            owner: None,
        }],
    });
    assert_eq!(second.await.unwrap().unwrap()[0].line, "other");
    assert!(lock(&client.inner.state).questions.is_empty());
    assert_eq!(journal.pending().unwrap(), []);
    assert_eq!(journal.claims.load(Ordering::Relaxed), 0);
    assert!(
        frames.try_recv().is_err(),
        "read replies need no acknowledgement"
    );
}

#[test]
fn disconnected_link_rejects_dispatch_before_claiming_or_queuing() {
    assert_not_sent(true);
}

#[tokio::test]
async fn lost_witness_release_reply_retries_only_cleanup_until_acknowledged() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(ClosingJournal {
        files: FileCallJournal::open(root.path()).unwrap(),
        closer: CancellationToken::new(),
        claims: AtomicU64::new(0),
    });
    let (client, mut frames) = connected(&journal);
    let releasing = client.clone();
    let release =
        tokio::spawn(async move { releasing.forget_owner_witnesses(&["disposed".into()]).await });
    let HostFrame::ForgetOwnerWitnesses { request_id, lines } = frames.recv().await.unwrap() else {
        panic!("witness release")
    };
    assert_eq!(lines, ["disposed"]);
    release.abort();
    assert!(release.await.unwrap_err().is_cancelled());
    let reading = client.clone();
    let independent =
        tokio::spawn(async move { reading.owner_floors(&["independent".into()]).await });
    let HostFrame::OwnerFloors {
        request_id: live, ..
    } = frames.recv().await.unwrap()
    else {
        panic!("independent floor read")
    };
    client.inner.retry_witness_releases();
    let HostFrame::ForgetOwnerWitnesses {
        request_id: retry,
        lines,
    } = frames.recv().await.unwrap()
    else {
        panic!("cleanup retry")
    };
    assert_eq!(retry, request_id);
    assert_eq!(lines, ["disposed"]);
    assert!(lock(&client.inner.state).questions.contains_key(&live));
    client
        .inner
        .receive(RunnerFrame::OwnerWitnessesForgotten { request_id });
    client.inner.retry_witness_releases();
    assert!(frames.try_recv().is_err());
    assert!(lock(&client.inner.state).witness_releases.is_empty());
    client.inner.receive(RunnerFrame::OwnerFloors {
        request_id: live,
        floors: vec![plakous_runner_protocol::OwnerFloor {
            line: "independent".into(),
            owner: None,
            retained: vec![],
        }],
    });
    assert_eq!(independent.await.unwrap().unwrap()[0].line, "independent");
    assert_eq!(journal.pending().unwrap(), []);
    assert_eq!(journal.claims.load(Ordering::Relaxed), 0);
}

#[test]
fn link_cancelled_during_journal_claim_is_unclaimed_without_queuing() {
    assert_not_sent(false);
}

#[tokio::test]
async fn refused_witness_release_does_not_retry_even_after_its_waiter_was_dropped() {
    for dropped in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let journal = Arc::new(ClosingJournal {
            files: FileCallJournal::open(root.path()).unwrap(),
            closer: CancellationToken::new(),
            claims: AtomicU64::new(0),
        });
        let (client, mut frames) = connected(&journal);
        let releasing = client.clone();
        let mut release = Some(tokio::spawn(async move {
            releasing.forget_owner_witnesses(&["disposed".into()]).await
        }));
        let HostFrame::ForgetOwnerWitnesses { request_id, .. } = frames.recv().await.unwrap()
        else {
            panic!("witness release")
        };
        if dropped {
            let release = release.take().unwrap();
            release.abort();
            assert!(release.await.unwrap_err().is_cancelled());
        }
        client.inner.receive(RunnerFrame::Refused {
            request_id: Some(request_id),
            refusal: Refusal::OwnerNotTransferred,
        });
        if let Some(release) = release {
            assert!(release.await.unwrap().is_err());
        }
        client.inner.retry_witness_releases();
        assert!(frames.try_recv().is_err());
        assert!(lock(&client.inner.state).witness_releases.is_empty());
        assert!(lock(&client.inner.state).questions.is_empty());
        assert_eq!(journal.claims.load(Ordering::Relaxed), 0);
    }
}

fn assert_not_sent(disconnect_first: bool) {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(ClosingJournal {
        files: FileCallJournal::open(root.path()).unwrap(),
        closer: CancellationToken::new(),
        claims: AtomicU64::new(0),
    });
    let (client, mut frames) = connected(&journal);
    if disconnect_first {
        client.disconnect();
    }
    let (answer, _answered) = oneshot::channel();
    let request = request();
    let outcome = client
        .inner
        .dispatch(request.clone(), HostFrame::Request(request), answer);
    assert!(
        matches!(outcome, Err(RunnerFailure::NotSent(_))),
        "{outcome:?}"
    );
    assert!(matches!(
        frames.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(lock(&client.inner.state).pending.is_empty());
    assert_eq!(journal.pending().unwrap(), []);
    assert_eq!(
        journal.claims.load(Ordering::Relaxed),
        u64::from(!disconnect_first)
    );
}
