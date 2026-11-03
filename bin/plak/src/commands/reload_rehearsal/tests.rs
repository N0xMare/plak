#![allow(
    clippy::too_many_lines,
    reason = "Reload rehearsal scenarios compare complete root and child projections with before/after read-only evidence."
)]

use super::*;

#[test]
fn oversized_transcript_projects_a_valid_suffix_without_content_in_report() {
    use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem};
    use plakous_hosted::sessions::{NoticeScope, TranscriptRow};
    let transcript = (0..MAX_SNAPSHOT_ROWS + 100)
        .map(|index| TranscriptItem {
            key: RowKey::new((index as u64 + 1) * 3, Lane::Product, 7),
            row: TranscriptRow::error(NoticeScope::Turn, "synthetic-private-text"),
        })
        .collect::<Vec<_>>();
    let row_count = transcript.len();
    let snapshot = plakous_hosted::runtime::rehearse_presentation_snapshot(transcript);
    assert_eq!(snapshot.items.len(), MAX_SNAPSHOT_ROWS);
    assert_eq!(snapshot.items[0].key, RowKey::new(303, Lane::Product, 7));
    assert_eq!(
        snapshot.items.last().unwrap().key,
        RowKey::new(row_count as u64 * 3, Lane::Product, 7)
    );
    let mut report = Vec::new();
    assert!(report_snapshot("root", row_count, &snapshot, &mut report).unwrap());
    assert!(
        !String::from_utf8(report)
            .unwrap()
            .contains("synthetic-private-text")
    );
}

#[test]
fn invalid_snapshot_refuses_without_echoing_content() {
    let snapshot = PresentationSnapshot {
        schema: u32::MAX,
        ..PresentationSnapshot::default()
    };
    assert!(!report_snapshot("root", 0, &snapshot, &mut Vec::new()).unwrap());
}

#[test]
fn fixture_home_without_session_is_refused_without_state_creation() {
    let home = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(home.path().to_path_buf());
    let mut output = Vec::new();
    assert!(rehearse(&paths, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", &mut output).is_err());
    assert_eq!(output, [] as [u8; 0]);
    assert!(!paths.state_root().exists());
}

#[tokio::test]
async fn fixture_projects_root_and_children_without_writes_or_content()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use plakous::sessions::{
        CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, text_runtime_binding};
    let fixture = tempfile::tempdir()?;
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let paths = PlakPaths::explicit(fixture.path().join("home"));
    paths.ensure_layout()?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        fixture.path().join("evidence"),
    )?;
    let binding = text_runtime_binding(
        &workspace,
        controller,
        "synthetic-private",
        "synthetic-private",
    )?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("rehearsal:fixture")?,
            binding,
        ))
        .await?
    else {
        return Err("fixture session was not created".into());
    };
    let mut record = crate::store::sessions::SessionRecord::new(workspace);
    record.plakous_locator = Some(session.locator());

    crate::store::sessions::save_session(&paths, &mut record)?;
    let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
        &record.id,
        record.workspace_root.clone(),
    );
    let child = tree
        .spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .map_err(|error| error.message)?;
    let child_controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        fixture.path().join("child-evidence"),
    )?;
    let child_binding = text_runtime_binding(
        &record.workspace_root,
        child_controller,
        "synthetic-private-child",
        "synthetic-private-child",
    )?;
    let CreateSessionOutcome::Created {
        session: child_session,
        ..
    } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("rehearsal:child")?,
            child_binding,
        ))
        .await?
    else {
        return Err("fixture child session was not created".into());
    };
    tree.bind_session_locator(&child.agent_id, child_session.locator().to_string())
        .map_err(|error| error.message)?;
    tree.set_node_status_for_test(
        &child.agent_id,
        plakous_hosted::agent_tree_protocol::NodeStatus::RecoveryRequired,
    );
    let pending = tree
        .spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .map_err(|error| error.message)?;
    tree.set_node_status_for_test(
        &pending.agent_id,
        plakous_hosted::agent_tree_protocol::NodeStatus::Pending,
    );
    let closed = tree
        .spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .map_err(|error| error.message)?;
    tree.bind_session_locator(
        &closed.agent_id,
        record.plakous_locator.unwrap().to_string(),
    )
    .map_err(|error| error.message)?;
    tree.close_agent(
        "root",
        &plakous_hosted::agent_tree_protocol::AttemptScopedRequest {
            agent_id: closed.agent_id.clone(),
            expected_attempt_id: None,
        },
        None,
    )
    .map_err(|error| error.message)?;
    crate::store::tree::save_tree(&paths, &mut tree).map_err(|error| error.message())?;
    let database = rusqlite::Connection::open(paths.subagents_database())?;
    database.execute(
        "UPDATE agent_nodes SET path=?1 WHERE agent_id=?2",
        rusqlite::params![
            fixture
                .path()
                .join("removed-worktree")
                .display()
                .to_string(),
            closed.agent_id
        ],
    )?;
    database.execute(
        "UPDATE agent_nodes SET active_turn=1,status=?1 WHERE agent_id=?2",
        rusqlite::params![
            plakous_hosted::agent_tree_protocol::NodeStatus::Running.as_str(),
            child.agent_id
        ],
    )?;
    let mut busy_output = Vec::new();
    assert!(rehearse(&paths, &record.id, &mut busy_output).is_err());
    let busy_text = String::from_utf8(busy_output)?;
    assert!(
        busy_text.contains(&format!("{}: busy: active_turn", child.agent_id)),
        "{busy_text}"
    );
    assert!(!busy_text.contains("synthetic-private"));
    database.execute(
        "UPDATE agent_nodes SET status=?1 WHERE agent_id=?2",
        rusqlite::params![
            plakous_hosted::agent_tree_protocol::NodeStatus::RecoveryRequired.as_str(),
            child.agent_id
        ],
    )?;
    let mut recovering_output = Vec::new();
    rehearse(&paths, &record.id, &mut recovering_output)
        .map_err(|error| error.message().to_owned())?;
    assert!(!String::from_utf8(recovering_output)?.contains("busy: active_turn"));
    database.execute(
        "UPDATE agent_nodes SET active_turn=0 WHERE agent_id=?1",
        [&child.agent_id],
    )?;
    database.execute(
        "UPDATE agent_nodes SET path=?1 WHERE agent_id=?2",
        rusqlite::params![
            fixture.path().join("missing-child").display().to_string(),
            child.agent_id
        ],
    )?;
    let mut broken_output = Vec::new();
    assert!(rehearse(&paths, &record.id, &mut broken_output).is_err());
    let broken_text = String::from_utf8(broken_output)?;
    assert!(broken_text.contains(&format!("{}: config=refused", child.agent_id)));
    assert!(!broken_text.contains("missing-child"));
    database.execute(
        "UPDATE agent_nodes SET path=?1 WHERE agent_id=?2",
        rusqlite::params![
            tree.node(&child.agent_id)
                .unwrap()
                .path
                .display()
                .to_string(),
            child.agent_id
        ],
    )?;
    drop(database);
    let mut output = Vec::new();
    rehearse(&paths, &record.id, &mut output).map_err(|error| error.message().to_owned())?;
    let text = String::from_utf8(output)?;
    assert!(text.contains("OpensQuiescent"), "{text}");
    assert!(text.contains("not rehearsed: needs credentials"));
    assert!(text.contains(&format!("{}: offline=OpensQuiescent", child.agent_id)));
    assert!(text.contains(&format!("{}: transcript_rows=", child.agent_id)));
    assert!(text.contains(&format!("{}: skipped:pending", pending.agent_id)));
    assert!(text.contains(&format!("{}: skipped:closed", closed.agent_id)));
    assert!(!text.contains("synthetic-private"));
    child_session
        .agent()
        .prompt("synthetic-private-child")
        .await?
        .result()
        .await?;
    let child_current = child_session.release().await?;
    let child_recorded = tree
        .node(&child.agent_id)
        .unwrap()
        .session_locator
        .as_deref()
        .unwrap()
        .parse::<SessionLocator>()?;
    session
        .agent()
        .prompt("synthetic-private")
        .await?
        .result()
        .await?;
    session.release().await?;
    let before_product = std::fs::read(paths.product_database())?;
    let before_runtime = std::fs::read(paths.runtime_database())?;
    let before_tree = std::fs::read(paths.subagents_database())?;
    let mut output = Vec::new();
    rehearse(&paths, &record.id, &mut output).map_err(|error| error.message().to_owned())?;
    let text = String::from_utf8(output)?;
    assert!(text.contains("projects:"));
    assert!(text.contains("root: offline=OpensQuiescent"));
    assert!(text.contains(&format!("{}: offline=OpensQuiescent", child.agent_id)));
    assert!(!text.contains("synthetic-private"));
    assert_eq!(before_product, std::fs::read(paths.product_database())?);
    assert_eq!(before_runtime, std::fs::read(paths.runtime_database())?);
    assert_eq!(before_tree, std::fs::read(paths.subagents_database())?);
    let followed = record.plakous_locator.unwrap();
    let (dry, projects, laws) = dry_run(&paths, followed, None, &ProductIntents::default())
        .map_err(|error| error.message().to_owned())?;
    assert!(projects > 0 && laws);
    let store: std::sync::Arc<dyn plakous_hosted::session_store::SessionRecordStore> =
        std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        ));
    plakous_hosted::runtime::project_session(&sessions, &store, &record).await?;
    let (_, _, intents, incremental) =
        crate::store::ProductStore::inspect_projection_read_only(&paths, &record.id)?;
    assert_eq!(incremental.as_ref(), Some(&dry));
    let (replayed, projects, laws) = dry_run(&paths, followed, incremental, &intents)
        .map_err(|error| error.message().to_owned())?;
    assert_eq!(projects, 0);
    assert!(laws);
    assert_eq!(replayed, dry);
    let mut database = crate::store::ProductStore::open(&paths)?;
    database.delete_views(&record.id)?;
    drop(database);
    let before = std::fs::read(paths.product_database())?;
    rehearse(&paths, &record.id, &mut Vec::new()).map_err(|error| error.message().to_owned())?;
    assert_eq!(before, std::fs::read(paths.product_database())?);
    let FeedPage::Records { mut records, .. } = AgentSessions::inspect_feed_sqlite(
        paths.runtime_database(),
        FeedPageRequest::after(FeedCursor::start(followed.session_id())),
    )?
    else {
        return Err("fixture feed unavailable".into());
    };
    let terminal = records
        .iter()
        .find(|record| {
            record
                .facts()
                .iter()
                .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
        })
        .ok_or("terminal missing")?;
    let mut header = *terminal.header();
    header.seq =
        plakous::sessions::feed::FeedSeq::new(records.last().unwrap().header().seq.get() + 1);
    let duplicate =
        plakous::sessions::feed::FeedRecord::from_store(header, terminal.facts().to_vec());
    records.push(duplicate);
    assert!(!terminal_law(&records, &dry, followed));
    assert_ne!(child_current, child_recorded);
    Ok(())
}
