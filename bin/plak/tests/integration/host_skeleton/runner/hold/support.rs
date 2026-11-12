//! Paired real-daemon fixtures and persisted oracles for placement holds.

use super::*;
use std::collections::BTreeSet;
use std::path::Path;

pub(super) struct Remote {
    pub(super) runner: ChildRunner,
    pub(super) path: String,
    pub(super) id: String,
    pub(super) cuts: PathBuf,
}

impl Remote {
    pub(super) fn new(fixture: &Fixture) -> Self {
        let initial = ChildRunner::of(fixture);
        std::fs::write(initial.home.join("runner/lease-clock-ms"), b"0").unwrap();
        let (runner, path, id) = pending_stop::remote(fixture);
        let cuts = fixture.root.join("hold-cuts");
        std::fs::create_dir_all(&cuts).unwrap();
        let proxy = fixture.root.join("hold-proxy.py");
        std::fs::write(&proxy, include_str!("../hold_proxy.py")).unwrap();
        let ssh = fixture.root.join("ssh-bin/ssh");
        let command = std::fs::read_to_string(&ssh).unwrap().replace(
            "exec \"$@\"",
            &format!(
                "exec python3 '{}' '{}' \"$@\"",
                proxy.display(),
                cuts.display()
            ),
        );
        std::fs::write(ssh, command).unwrap();
        Self {
            runner,
            path,
            id,
            cuts,
        }
    }

    pub(super) fn script(&self, fixture: &Fixture) -> serde_json::Value {
        json!({"version": 1, "process_tools": true, "identity_namespace": 781,
        "runner_windows": {"read_ms": 600, "effect_ms": 1600, "ping_ms": 50, "dead_ms": 250},
        "runner_hold_cuts": self.cuts,
        "steps": [
            {"output": {"tool": {"name": "process_session_start", "arguments": {
                "command": format!("printf '%s %s\\n' \"$$\" \"$PPID\" >> '{}'; n=0; while [ ! -f '{}' ]; do [ \"$n\" -lt 30000 ] || exit 1; n=$((n + 1)); sleep 0.01; done; printf done > '{}'", fixture.root.join("local-job.marker").display(), fixture.root.join("foreground-release").display(), fixture.root.join("foreground-completed").display()),
                "workdir": ".", "login": false, "tty": false, "keep_stdin_open": false,
                "yield_time_ms": 10000, "max_output_bytes": 8192
            }}}, "finish": "tool_calls"},
            {"output": {"text": ANSWER}, "finish": "stop"}
        ]})
    }

    pub(super) fn start(&self, fixture: &Fixture) -> (Host, Client, String, NodeAddress) {
        let host = start_env(fixture, &[], &[("PATH", self.path.clone())]);
        let mut client = Client::connect(&host);
        let mut request = new_session(&fixture.workspace());
        if let Op::NewSession { runner, .. } = &mut request {
            *runner = plakous_host_protocol::wire::RunnerLabel {
                name: "remote".into(),
                runner_id: self.id.clone(),
            };
        }
        // The host's startup reconciliation of the paired runner is a bounded,
        // documented not-ready window; only that refusal is retried.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut id = 1;
        let created = loop {
            let created = client.request(id, request.clone());
            let reconciling = format!("{created:?}")
                .contains("RunnerUnreachable { phase: Reconcile, timed_out: false }");
            if !reconciling || std::time::Instant::now() >= deadline {
                break created;
            }
            id += 100;
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        let Outcome::Ok(Answer::Created { session, node, .. }) = created else {
            panic!("remote creation refused: {created:?}")
        };
        let attachment = client.attach(2, &node);
        assert!(matches!(
            client.request(
                3,
                Op::Intent {
                    client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                    attachment,
                    intent: SessionIntent::Prompt(Draft::from("placement hold proof")),
                    blobs: Vec::new(),
                }
            ),
            Outcome::Ok(Answer::Accepted { .. })
        ));
        (host, client, session, node)
    }

    pub(super) fn arm(&self, name: &str) {
        std::fs::write(self.cuts.join(format!("{name}.armed")), b"armed").unwrap();
    }

    pub(super) fn release(&self, name: &str) {
        std::fs::write(self.cuts.join(format!("{name}.release")), b"release").unwrap();
    }

    pub(super) fn entered(&self, name: &str) {
        wait_file(&self.cuts.join(format!("{name}.entered")));
    }
}

pub(super) fn wait_file(path: &Path) {
    until("fixture marker", || path.is_file());
}

pub(super) fn until(label: &str, predicate: impl FnMut() -> bool) {
    until_within(label, WAIT, predicate);
}

pub(super) fn until_within(
    label: &str,
    limit: std::time::Duration,
    mut predicate: impl FnMut() -> bool,
) {
    let deadline = std::time::Instant::now() + limit;
    while !predicate() {
        assert!(std::time::Instant::now() < deadline, "timed out: {label}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

pub(super) fn row(client: &mut Client, session: &str) -> SessionRow {
    client
        .rows(71)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
}

pub(super) fn held(client: &mut Client, session: &str) -> SessionRow {
    until("resident placement hold", || {
        let row = row(client, session);
        row.activity == SessionActivity::Busy
            && row.needs_operator
            && matches!(
                row.runner_status,
                plakous_host_protocol::wire::RunnerStatus::Blocked { .. }
            )
    });
    row(client, session)
}

pub(super) fn idle(client: &mut Client, session: &str) {
    until("one terminal", || {
        row(client, session).activity != SessionActivity::Busy
    });
}

pub(super) fn providers(fixture: &Fixture) -> usize {
    plakous_offline_crash_harness::evidence::read_evidence(fixture.root.join("evidence.jsonl"))
        .unwrap()
        .iter()
        .filter(|row| {
            row.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
        })
        .count()
}

pub(super) fn assert_waiting(fixture: &Fixture, client: &mut Client, session: &str) {
    assert_eq!(
        providers(fixture),
        1,
        "held work dispatched another provider round"
    );
    assert_eq!(
        unknown_effects(fixture, session),
        0,
        "safe live work became unknown"
    );
    assert_eq!(row(client, session).activity, SessionActivity::Busy);
    assert_eq!(snapshot(fixture, session).terminals, 0);
    if fixture.root.join("local-job.marker").exists() {
        job(fixture);
    }
}

pub(super) fn finish(
    fixture: &Fixture,
    remote: &Remote,
    client: &mut Client,
    session: &str,
) -> Snapshot {
    remote.release("before-ready");
    remote.release("before-answer");
    std::fs::write(fixture.root.join("foreground-release"), b"release").unwrap();
    idle(client, session);
    wait_file(&fixture.root.join("foreground-completed"));
    assert_eq!(
        std::fs::read(fixture.root.join("foreground-completed")).unwrap(),
        b"done"
    );
    job(fixture);
    assert_eq!(providers(fixture), 2);
    assert_eq!(unknown_effects(fixture, session), 0);
    let snapshot = snapshot(fixture, session);
    assert_eq!(snapshot.terminals, 1);
    assert_eq!(
        snapshot.job_terminals, 0,
        "foreground was promoted or cleanup fabricated a job"
    );
    snapshot
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Snapshot {
    pub(super) operations: BTreeSet<(u64, String, String)>,
    pub(super) terminals: usize,
    pub(super) job_terminals: i64,
}

pub(super) fn snapshot(fixture: &Fixture, session: &str) -> Snapshot {
    use plak::store::session_records::SessionRecordStore as _;
    use plakous::sessions::feed::{
        FeedCursor, FeedFact, FeedOperationKind, FeedPage, FeedPageRequest, FeedSeq,
    };
    use plakous::sessions::{AgentSessions, SessionBackend};
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let locator = plak::store::session_records::SqliteSessionStore::new(paths.clone())
        .load(session)
        .unwrap()
        .plakous_locator
        .unwrap();
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let job_terminals = rusqlite::Connection::open(paths.runtime_database())
        .unwrap()
        .query_row("SELECT count(*) FROM line_job_terminals", [], |row| {
            row.get(0)
        })
        .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut cursor = FeedSeq::new(0);
            let mut snapshot = Snapshot {
                operations: BTreeSet::new(),
                terminals: 0,
                job_terminals,
            };
            loop {
                let FeedPage::Records { records, .. } = sessions
                    .feed_page(FeedPageRequest::after(FeedCursor::new(
                        locator.session_id(),
                        cursor,
                    )))
                    .await
                    .unwrap()
                else {
                    panic!("trimmed fixture feed")
                };
                if records.is_empty() {
                    return snapshot;
                }
                for record in records {
                    for fact in record.facts() {
                        match fact {
                            FeedFact::OperationAdvanced {
                                operation_id,
                                kind,
                                state,
                                ..
                            } => {
                                let kind = match kind {
                                    FeedOperationKind::Effect { .. } => "effect",
                                    FeedOperationKind::CodeCell { .. } => "cell",
                                    FeedOperationKind::ProviderAttempt { .. } => "provider",
                                    _ => "other",
                                };
                                snapshot.operations.insert((
                                    *operation_id,
                                    kind.into(),
                                    format!("{state:?}"),
                                ));
                            }
                            FeedFact::TurnTerminal { .. } => snapshot.terminals += 1,
                            _ => {}
                        }
                    }
                    cursor = record.header().seq;
                }
            }
        })
}
