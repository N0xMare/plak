//! A cancelled transport is not a dispatch target, even before teardown drains it.

#![allow(clippy::unwrap_used, reason = "isolated dispatch and journal fixture")]

use super::*;
use crate::FileCallJournal;
use std::io;

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

#[test]
fn disconnected_link_rejects_dispatch_before_claiming_or_queuing() {
    assert_not_sent(true);
}

#[test]
fn link_cancelled_during_journal_claim_is_unclaimed_without_queuing() {
    assert_not_sent(false);
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
