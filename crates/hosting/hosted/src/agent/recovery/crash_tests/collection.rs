//! Real tree and runtime commits across finished-child retirement.

use super::*;
use plakous::sessions::{
    OpenSessionOutcome, OpenSessionRequest, SessionLocator, SessionOperationId,
};
use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};

const COLLECT: &str = "finished-collection";
const ARCHIVE_ID: &str = "fixture-artifact";

fn artifact_digest() -> String {
    "a".repeat(64)
}

async fn seed(root: &Path) -> (String, SessionLocator, SessionLocator) {
    let fixture = Fixture::new(root, COLLECT, None, false);
    let sessions = fixture.parent.state_roots().runtime_sessions().unwrap();
    let create = async |id| {
        let request =
            CreateSessionRequest::new(SessionOperationId::new(id).unwrap(), fixture.binding());
        let CreateSessionOutcome::Created { session, .. } = sessions.create(request).await.unwrap()
        else {
            panic!("creation did not mint a session")
        };
        session.release().await.unwrap()
    };
    let parent_locator = create("collection:parent").await;
    let child_locator = create("collection:child").await;
    let child = fixture
        .runtime
        .mutate_then_emit(false, move |tree| {
            let request = SpawnAgentRequest::test_review();
            let child = tree.spawn_agent("root", &request, None).unwrap().agent_id;
            tree.bind_session_locator(&child, child_locator.to_string())
                .unwrap();
            let turn_token = tree.attempt(&child).unwrap().turn_token;
            tree.submit_result(
                &child,
                &crate::agent_tree_protocol::SubmitResultRequest {
                    turn_token,
                    output_json: r#"{"ok":true}"#.to_owned(),
                },
                Some("collection:submit"),
            )
            .unwrap();
            tree.record_turn_completed(&child).unwrap();
            tree.settle_for_test();
            let mail = tree.pending_result_mail("root").remove(0);
            tree.acknowledge_mail(&mail.message_id).unwrap();
            tree.attempt_mut_for_test(&child).ended_at = Some(1);
            Ok(child)
        })
        .await
        .unwrap();
    fixture
        .runtime
        .collect_dormant_children(&sessions, true, true, None, None)
        .await
        .unwrap();
    assert!(
        fixture
            .runtime
            .read(|tree| tree.retired_children().is_empty())
            .unwrap()
    );
    assert!(sessions.head_is_quiescent(child_locator).await.unwrap());
    let revision = sessions
        .child_retirement_revision(child_locator)
        .await
        .unwrap();
    SqliteTreeStore::open(&root.join("tree.sqlite3"))
        .unwrap()
        .record_child_archive_receipt(
            &fixture.tree_id,
            &child,
            &crate::agent_tree::ChildArchiveReceipt {
                id: ARCHIVE_ID.to_owned(),
                sha256: artifact_digest(),
            },
            |_| Ok(revision),
        )
        .unwrap();
    crate::topology_runtime::force_release(&fixture.tree_id).await;
    (child, parent_locator, child_locator)
}

fn owner(root: &Path, cut: Option<&str>) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "agent::recovery::crash_tests::collection::collector_process",
            "--ignored",
            "--nocapture",
        ])
        .env(ROOT_ENV, root)
        .env(TRACE_ENV, root.join("collection-commits.jsonl"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cut) = cut {
        command.env(KILL_ENV, cut);
    }
    command.spawn().unwrap()
}

#[test]
#[ignore = "subprocess helper for finished-child collection crash cuts"]
fn collector_process() {
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).unwrap());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let fixture = Fixture::new(&root, COLLECT, None, true);
        let sessions = fixture.parent.state_roots().runtime_sessions().unwrap();
        if root.join("before-intent").exists() {
            cut_ready("before-intent").await;
        }
        fixture
            .runtime
            .collect_dormant_children(&sessions, true, true, None, None)
            .await
            .unwrap();
        sessions.reclaim().await.unwrap();
        crate::topology_runtime::force_release(&fixture.tree_id).await;
    });
}

fn cut_for(rows: &[serde_json::Value], store: &str, table: &str) -> String {
    let row = rows
        .iter()
        .find(|row| {
            row["store"] == store
                && row["indexed"] == true
                && row["tables"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == table)
        })
        .unwrap_or_else(|| panic!("no {store} commit updated {table}"));
    format!("{store}:{}", row["store_ordinal"].as_u64().unwrap())
}

#[test]
fn finished_child_retirement_sigkill_cuts_preserve_the_result_and_parent() {
    let pilot = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(seed(pilot.path()));
    let output = owner(pilot.path(), None).wait_with_output().unwrap();
    let trace = pilot.path().join("collection-commits.jsonl");
    assert!(
        output.status.success(),
        "{}; commit trace: {}",
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(&trace).unwrap_or_default()
    );
    let rows = trace_rows(&trace);
    for (index, cut) in [
        None,
        Some(cut_for(&rows, "tree", "agent_retired_children")),
        Some(cut_for(&rows, "runtime", "archive_receipts")),
        Some(cut_for(&rows, "runtime", "delete_tombstones")),
        Some(cut_for(&rows, "runtime", "runtime_object_bytes")),
    ]
    .into_iter()
    .enumerate()
    {
        let scratch = tempfile::tempdir().unwrap();
        let (child, parent, removed) = rt.block_on(seed(scratch.path()));
        if cut.is_none() {
            std::fs::write(scratch.path().join("before-intent"), "").unwrap();
        }
        let mut owner = owner(scratch.path(), cut.as_deref());
        if cut.is_none() {
            assert_eq!(await_owner(&mut owner, COLLECT), "before-intent");
            owner.kill().unwrap();
        }
        let output = owner.wait_with_output().unwrap();
        assert_eq!(
            output.status.signal(),
            Some(9),
            "{cut:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        rt.block_on(assert_recovery(
            scratch.path(),
            index,
            &child,
            parent,
            removed,
        ));
    }
}

#[test]
fn child_retirement_defers_on_an_older_archive_receipt_for_the_same_head() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (child, _, locator) = seed(root).await;
        let fixture = Fixture::new(root, COLLECT, None, true);
        let sessions = fixture.parent.state_roots().runtime_sessions().unwrap();
        let revision = sessions.child_retirement_revision(locator).await.unwrap();
        // Advance only the revision in the fixture. A real branch fork would
        // also pin the session and mask a broken receipt check.
        let db = rusqlite::Connection::open(root.join("plak/runtime.sqlite3")).unwrap();
        assert_eq!(
            db.execute(
                "UPDATE sessions SET revision=?1 WHERE session_id=?2",
                rusqlite::params![
                    (revision + 1).to_be_bytes(),
                    locator.session_id().to_string()
                ],
            )
            .unwrap(),
            1
        );
        assert_eq!(
            sessions.child_retirement_revision(locator).await.unwrap(),
            revision + 1
        );
        fixture
            .runtime
            .collect_dormant_children(&sessions, true, true, None, None)
            .await
            .unwrap();
        assert!(sessions.head_is_quiescent(locator).await.unwrap());
        fixture
            .runtime
            .read(|tree| assert!(!tree.retired_children().contains_key(&child)))
            .unwrap();
        crate::topology_runtime::force_release(&fixture.tree_id).await;
    });
}

async fn assert_recovery(
    root: &Path,
    cut: usize,
    child: &str,
    parent: SessionLocator,
    removed: SessionLocator,
) {
    let fixture = Fixture::new(root, COLLECT, None, true);
    let sessions = fixture.parent.state_roots().runtime_sessions().unwrap();
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(tree.retired_children().contains_key(child), cut > 0);
            assert_eq!(
                tree.pending_result_mail("root"),
                [] as [crate::agent_tree::MailRecord; 0]
            );
        })
        .unwrap();
    assert_eq!(sessions.head_is_quiescent(removed).await.is_ok(), cut < 2);
    if cut == 1 {
        // Simulate bad local evidence after intent publication. A previously
        // guarded intent must not dispatch even if the setting is now off.
        let db = rusqlite::Connection::open(root.join("tree.sqlite3")).unwrap();
        let set_digest = |digest: &str| {
            assert_eq!(
                db.execute(
                    "UPDATE agent_child_archive_receipts SET sha256=?1",
                    [digest]
                )
                .unwrap(),
                1
            );
        };
        set_digest("invalid");
        fixture
            .runtime
            .collect_dormant_children(&sessions, false, false, None, None)
            .await
            .unwrap();
        assert!(sessions.head_is_quiescent(removed).await.unwrap());
        set_digest(&artifact_digest());
    }
    assert_eq!(
        plakous::sessions::AgentSessions::child_retirement_can_abort_sqlite(
            root.join("plak/runtime.sqlite3"),
            removed,
        )
        .unwrap(),
        cut < 2
    );
    fixture
        .runtime
        .collect_dormant_children(&sessions, true, true, None, None)
        .await
        .unwrap();
    fixture
        .runtime
        .collect_dormant_children(&sessions, false, true, None, None)
        .await
        .unwrap();
    sessions.reclaim().await.unwrap();
    fixture
        .runtime
        .read(|tree| {
            assert!(tree.node(child).is_err());
            let summary = &tree.retired_children()[child];
            assert_eq!(summary.output.as_deref(), Some(r#"{"ok":true}"#));
            assert!(summary.mail.is_some());
            let archive = summary.archive.as_ref().unwrap();
            assert_eq!(archive.id, ARCHIVE_ID);
            assert_eq!(archive.sha256, artifact_digest());
            assert_eq!(
                tree.pending_result_mail("root"),
                [] as [crate::agent_tree::MailRecord; 0]
            );
        })
        .unwrap();
    let request = OpenSessionRequest::new(
        SessionOperationId::new("collection:reopen-parent").unwrap(),
        parent,
        fixture.binding(),
    );
    let OpenSessionOutcome::Opened { session, .. } = sessions.open(request).await.unwrap() else {
        panic!("the parent's exact session was not reopenable")
    };
    session.release().await.unwrap();
    assert!(sessions.head_is_quiescent(removed).await.is_err());
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}
