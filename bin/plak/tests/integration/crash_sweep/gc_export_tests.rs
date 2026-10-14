//! `plak session gc` and `plak session export` run the projector first (spec
//! 02 §2.5.1, §2.5.3): they read the head the feed names, and a workflow
//! request without a feed fact keeps its source checkpoint.

use super::*;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};
use plakous_hosted::sessions::projector::{SessionViews, WorkflowRequest};
use plakous_hosted::sessions::{ProductCompactReason, SessionWorkflowKind};
use std::time::Duration;

fn views(fixture: &Fixture, session: &str) -> SessionViews {
    SqliteSessionStore::new(summary::paths(fixture))
        .load_views(session)
        .unwrap()
        .expect("the session has views")
}

fn compact_rows(views: &SessionViews) -> usize {
    views
        .rows
        .values()
        .filter(|row| row.as_tool().is_some_and(|tool| tool.name == "compact"))
        .count()
}

/// Runs `plak session gc --all` until the killed owner's lease has expired.
fn gc_all(fixture: &Fixture, session: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let output = fixture
            .command()
            .args(["session", "gc", "--all"])
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            output.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if !stdout.contains(&format!("skipped {session}: it is open in a running plak")) {
            return stdout;
        }
        assert!(
            Instant::now() < deadline,
            "the killed owner's lease never expired"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Runs `plak session recover` until the killed owner's lease has expired.
fn recover(fixture: &Fixture, session: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let output = fixture
            .command()
            .args(["session", "recover", session])
            .output()
            .unwrap();
        if output.status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "recover never succeeded: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn export(fixture: &Fixture, session: &str) -> std::process::Output {
    let out = fixture.root.join("gc-export.json");
    fixture
        .command()
        .args(["session", "export", session, "--out"])
        .arg(&out)
        .output()
        .unwrap()
}

/// The host is killed right after the compaction's runtime commits, before a
/// projector transaction applies them. `gc --all` applies the feed itself and
/// skips the killed owner's Working head as needing recovery; after `plak
/// session recover` it prunes at the head the views then name, and the
/// relaunch shows the compaction once.
#[test]
fn gc_after_a_compaction_cut_before_projection_prunes_at_the_compacted_head() {
    let shape = "scheduled_compaction_workflow";
    let (seed, session) = setup(shape);
    let commits = pilot(&seed, &session, shape);
    let target = commits
        .iter()
        .rfind(|cut| cut.store == "runtime" && (9..=11).contains(&cut.store_ordinal))
        .unwrap();
    let (fixture, _) = kill_fixture(&seed, &session, shape, &commits, target);
    let cursor = views(&fixture, &session).cursor;

    let mut out = gc_all(&fixture, &session);
    assert!(views(&fixture, &session).cursor >= cursor);
    if out.contains(&format!("skipped {session}: its turn is unfinished")) {
        recover(&fixture, &session);
        out = gc_all(&fixture, &session);
    }
    assert!(!out.contains(&format!("skipped {session}")), "{out}");
    let applied = views(&fixture, &session);
    let head = applied.state.head.expect("the views name a head");
    let recovery = plakous::sessions::AgentSessions::inspect_recovery_sqlite(
        summary::paths(&fixture).runtime_database(),
        plakous::sessions::SessionLocator::new(head.session_id, head.branch_id, head.checkpoint_id),
    );
    assert!(
        matches!(recovery, Ok(None)),
        "gc pruned at a head that is not the quiescent tail: {recovery:?}"
    );

    let mut opened = traced_reopen(&fixture, &session, "after-gc", None)
        .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
    opened.send(&quit_frame());
    assert!(opened.finish().status.success());
    assert_eq!(compact_rows(&views(&fixture, &session)), 1);
    let intents = SqliteSessionStore::new(summary::paths(&fixture))
        .load_intents(&session)
        .unwrap();
    let reopened = views(&fixture, &session);
    assert!(
        intents
            .workflow_requests
            .iter()
            .all(|request| reopened.state.workflows.contains_key(&request.operation_id)),
        "a workflow request was left pending"
    );
}

/// A workflow request with no feed fact still names its source checkpoint:
/// `gc --all` skips the session as `workflow pending` and export refuses it,
/// until the request is settled or withdrawn.
#[test]
fn a_workflow_request_without_a_fact_holds_gc_and_export() {
    let (fixture, session) = setup("final_only");
    let head = views(&fixture, &session)
        .state
        .head
        .expect("the views name a head");
    let request = WorkflowRequest {
        operation_id: "plak:test-unadmitted-compact".to_owned(),
        kind: SessionWorkflowKind::Compact(ProductCompactReason::Manual),
        source_locator: plakous::sessions::SessionLocator::new(
            head.session_id,
            head.branch_id,
            head.checkpoint_id,
        ),
    };
    let mut store = plak::store::ProductStore::open(&summary::paths(&fixture)).unwrap();
    store.write_workflow_request(&session, &request).unwrap();

    let out = gc_all(&fixture, &session);
    assert!(
        out.contains(&format!("skipped {session}: workflow pending")),
        "{out}"
    );
    assert!(out.starts_with("reclaimed 0 checkpoints"), "{out}");
    let refused = export(&fixture, &session);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(&format!(
            "session {session} cannot be exported: a command submission or workflow request has no feed fact yet; open the session once to settle it"
        )),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let mut record = store.load(&session).unwrap();
    let notices = store.withdraw_unadmitted_workflows(&mut record).unwrap();
    assert_eq!(notices.len(), 1);
    assert!(
        store
            .withdraw_unadmitted_workflows(&mut record)
            .unwrap()
            .is_empty()
    );
    let out = gc_all(&fixture, &session);
    assert!(!out.contains(&format!("skipped {session}")), "{out}");
    let exported = export(&fixture, &session);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
}
