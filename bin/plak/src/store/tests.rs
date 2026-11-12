use super::*;
use plakous_hosted::sessions::NoticeScope;
use plakous_hosted::sessions::projector::{
    Delivery, SessionViews, Submission, SubmissionKind, ViewWrites,
};
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn doctor_reports_foreign_key_violation_but_open_admits_schema() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    drop(ProductStore::open(&paths).unwrap());
    let path = product_sqlite_path(&paths).unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys=OFF; INSERT INTO prompt_history VALUES ('missing',0,'test');",
        )
        .unwrap();
    drop(connection);
    assert!(ProductStore::open(&paths).is_ok());
    assert!(matches!(
        crate::product_doctor::inspect_state_database(&path, SCHEMA_VERSION),
        crate::product_doctor::DatabaseStatus::Corrupt(_)
    ));
}

#[test]
fn doctor_reports_corrupt_pages_and_store_open_returns_an_error() {
    use plakous_recursive_subagents_store::database::SubagentsDatabase;
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    drop(ProductStore::open(&paths).unwrap());
    let product = product_sqlite_path(&paths).unwrap();
    let tree = root.path().join("subagents.sqlite3");
    drop(SubagentsDatabase::open(&tree).unwrap());
    for (path, version) in [
        (&product, SCHEMA_VERSION),
        (&tree, plakous_recursive_subagents_store::SCHEMA_VERSION),
    ] {
        let mut bytes = std::fs::read(path).unwrap();
        // Damage the sqlite_schema b-tree, leaving the database header intact.
        bytes[100] = 0xff;
        std::fs::write(path, bytes).unwrap();
        let status = crate::product_doctor::inspect_state_database(path, version);
        assert!(
            matches!(status, crate::product_doctor::DatabaseStatus::Corrupt(_)),
            "{status:?}"
        );
    }
    assert!(ProductStore::open(&paths).is_err());
    assert!(SubagentsDatabase::open(&tree).is_err());
}

#[cfg(feature = "test-hooks")]
#[test]
fn commit_trace_frameless_product_ops_release_for_another_thread() {
    use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};
    for (mode, expected) in [("acknowledge", 3), ("delete", 2)] {
        let root = tempdir().unwrap();
        let trace = root.path().join("trace.jsonl");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::tests::commit_trace_frameless_product_child",
                "--nocapture",
            ])
            .env("PLAK_SW1_PRODUCT_FRAMELESS_ROOT", root.path())
            .env("PLAK_SW1_PRODUCT_FRAMELESS_MODE", mode)
            .env(TRACE_ENV, &trace)
            .env_remove(KILL_ENV)
            .output()
            .unwrap();
        assert!(output.status.success(), "{mode}: {output:?}");
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(trace)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), expected);
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(line["store"], "product");
            assert_eq!(line["ordinal"], index + 1);
        }
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn commit_trace_frameless_product_child() {
    let Some(root) = std::env::var_os("PLAK_SW1_PRODUCT_FRAMELESS_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mut store = ProductStore::open(&PlakPaths::explicit(root.join("p"))).unwrap();
    if std::env::var("PLAK_SW1_PRODUCT_FRAMELESS_MODE").unwrap() == "acknowledge" {
        let ack = crate::surface_protocol::AgentAcknowledgement {
            agent_id: "agent".to_owned(),
            attempt_id: "attempt".to_owned(),
            attention: "failed".to_owned(),
            ended_at: Some(10),
        };
        store.acknowledge_agent_attempt("tree", &ack, 10).unwrap();
        store.acknowledge_agent_attempt("tree", &ack, 20).unwrap();
    } else {
        store.delete_agent_acknowledgements("absent").unwrap();
    }
    std::thread::spawn(move || {
        let mut peer = ProductStore::open(&PlakPaths::explicit(root.join("p"))).unwrap();
        peer.commit(&mut SessionRecord::new(root)).unwrap();
    })
    .join()
    .unwrap();
}

#[cfg(feature = "test-hooks")]
#[test]
fn commit_trace_product_and_tree_share_one_process_sequence() {
    use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};
    let root = tempdir().unwrap();
    let trace = root.path().join("trace.jsonl");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "store::tests::commit_trace_product_and_tree_child",
            "--nocapture",
        ])
        .env("PLAK_SW1_PRODUCT_TRACE_ROOT", root.path())
        .env(TRACE_ENV, &trace)
        .env_remove(KILL_ENV)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(lines.iter().any(|line| line["store"] == "product"));
    assert!(lines.iter().any(|line| line["store"] == "tree"));
    assert!(lines.iter().any(|line| {
        line["tables"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("sessions"))
    }));
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line["ordinal"], index + 1);
        assert_eq!(line["indexed"], true);
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn commit_trace_product_and_tree_child() {
    let Some(root) = std::env::var_os("PLAK_SW1_PRODUCT_TRACE_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let paths = PlakPaths::explicit(root.join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.clone());
    store.commit(&mut session).unwrap();
    let _tree =
        plakous_recursive_subagents_store::SqliteTreeStore::open(&root.join("tree.sqlite3"))
            .unwrap();
    session.append_transcript(TranscriptRow::user("trace fixture"));
    store.commit(&mut session).unwrap();
}

/// Commits `rows` as `session_id`'s projector rows (anchor 1, lane 0),
/// replacing any views it had: the store side of a projector run.
fn project_rows(store: &mut ProductStore, session_id: &str, rows: &[TranscriptRow]) {
    use plakous_hosted::sessions::projector::{Lane, RowKey, SessionViews, ViewWrites};
    store.delete_views(session_id).unwrap();
    let mut views = SessionViews::empty();
    views.cursor = plakous::sessions::feed::FeedSeq::new(1);
    for (ordinal, row) in rows.iter().enumerate() {
        views.rows.insert(
            RowKey::new(1, Lane::Runtime, u32::try_from(ordinal).unwrap()),
            row.clone(),
        );
    }
    let writes = ViewWrites {
        expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
        cursor: views.cursor,
        upserts: views
            .rows
            .iter()
            .map(|(key, row)| (*key, row.clone()))
            .collect(),
        deletes: Vec::new(),
        calls: Vec::new(),
        open_turns: Vec::new(),
        processes: Vec::new(),
        successors: Vec::new(),
    };
    assert_eq!(
        store
            .commit_projection_views(session_id, &writes, &views.state)
            .unwrap(),
        plakous_hosted::sessions::projector::ProjectionCommit::Applied
    );
}

/// Every stored row type, with every optional fact populated, reloads as
/// the same typed row from a fresh store open: through the projector's
/// view commit for runtime rows and as operator notice intents.
#[test]
fn every_transcript_row_type_round_trips_through_the_store() {
    use plakous_hosted::sessions::projector::{Lane, RowKey, SessionViews, ViewWrites};
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let rows = every_stored_row_type();
    let mut views = SessionViews::empty();
    views.cursor = plakous::sessions::feed::FeedSeq::new(1);
    for (ordinal, row) in rows.iter().enumerate() {
        views.rows.insert(
            RowKey::new(1, Lane::Runtime, u32::try_from(ordinal).unwrap()),
            row.clone(),
        );
    }
    {
        let mut store = ProductStore::open(&paths).unwrap();
        store.commit(&mut session).unwrap();
        let writes = ViewWrites {
            expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
            cursor: views.cursor,
            upserts: views
                .rows
                .iter()
                .map(|(key, row)| (*key, row.clone()))
                .collect(),
            deletes: Vec::new(),
            calls: Vec::new(),
            open_turns: Vec::new(),
            processes: Vec::new(),
            successors: Vec::new(),
        };
        store
            .commit_projection_views(&session.id, &writes, &views.state)
            .unwrap();
        session.record_notice(TranscriptRow::warning(NoticeScope::Tree, "a notice intent"));
        store.commit(&mut session).unwrap();
    }
    let loaded = ProductStore::open(&paths)
        .unwrap()
        .load(&session.id)
        .unwrap();
    let mut expected = rows;
    expected.push(TranscriptRow::warning(NoticeScope::Tree, "a notice intent"));
    assert_eq!(
        loaded.transcript.iter().cloned().collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn a_held_product_writer_refuses_without_partial_binding_and_retries_after_release() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut writer = Connection::open(paths.product_database()).unwrap();
    let transaction = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    store.connection.busy_timeout(Duration::ZERO).unwrap();
    let mut session = SessionRecord::new(root.path().join("workspace"));
    assert_eq!(
        store.commit(&mut session).map_err(String::from),
        Err("product store is busy".to_owned())
    );
    assert_eq!(session.store_revision, 0);
    assert!(store.load(&session.id).is_err());
    let bindings: i64 = transaction
        .query_row("SELECT COUNT(*) FROM identity_bindings", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(bindings, 0);
    transaction.rollback().unwrap();
    store.commit(&mut session).unwrap();
    assert_eq!(session.store_revision, 1);
    assert_eq!(store.load(&session.id).unwrap().id, session.id);
}

#[test]
fn a_held_product_writer_refuses_a_submission_as_typed_busy() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().join("workspace"));
    store.commit(&mut session).unwrap();
    let item = QueueItem::new("wake");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "wake-command");
    let mut writer = Connection::open(paths.product_database()).unwrap();
    let transaction = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    store.connection.busy_timeout(Duration::ZERO).unwrap();
    let before = session.clone();
    let refused = store.commit_submission(&mut session, &request).unwrap_err();
    assert_eq!(refused.refusal(), Some(StoreRefusal::Busy));
    assert_eq!(refused.message(), "product store is busy");
    assert_eq!(session, before);
    transaction.rollback().unwrap();
    store.commit_submission(&mut session, &request).unwrap();
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        [request]
    );
}

#[test]
fn read_only_session_inspection_sees_live_wal_without_creating_or_writing_state() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    let unknown = "00000000-0000-4000-8000-000000000001";
    assert!(ProductStore::inspect_session_read_only(&paths, unknown).is_err());
    assert!(!paths.product_database().exists());
    assert!(!root.path().join("home").exists());

    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().join("workspace"));
    session.account = Some("codex".to_owned());
    store.commit(&mut session).unwrap();
    let wal = PathBuf::from(format!("{}-wal", paths.product_database().display()));
    assert!(wal.metadata().unwrap().len() > 0);
    let before_version: i64 = store
        .connection
        .query_row("PRAGMA schema_version", [], |row| row.get(0))
        .unwrap();
    let before_fingerprint: String = store
        .connection
        .query_row(
            "SELECT fingerprint FROM product_schema WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let before_revision = session.store_revision;
    let inspected = ProductStore::inspect_session_read_only(&paths, &session.id).unwrap();
    assert_eq!(inspected.id, session.id);
    assert_eq!(inspected.account, session.account);
    assert_eq!(inspected.store_revision, before_revision);
    assert_eq!(
        store.load(&session.id).unwrap().store_revision,
        before_revision
    );
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        before_version
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT fingerprint FROM product_schema WHERE singleton=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        before_fingerprint
    );
}

fn assert_exact_schema(paths: &PlakPaths) {
    let connection = Connection::open(paths.product_database()).unwrap();
    SCHEMA.validate(&connection).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
}

fn submission(item: &QueueItem, command: &str) -> Submission {
    Submission {
        command_id: command.into(),
        kind: SubmissionKind::RootStart,
        queue_item_id: item.id.clone(),
        queue_item_revision: item.revision,
        target_turn_id: None,
        canonical_command: item.text.clone(),
        submitted_at_ms: item.submitted_at_ms,
    }
}

fn project_delivery(store: &mut ProductStore, id: &str, command: &str, delivery: Delivery) {
    let before = store
        .load_views(id)
        .unwrap()
        .unwrap_or_else(SessionViews::empty);
    let mut views = before.clone();
    views.state.deliveries.insert(command.into(), delivery);
    store
        .commit_projection_views(id, &ViewWrites::between(&before, &views), &views.state)
        .unwrap();
}

fn append_settled_submission(
    store: &mut ProductStore,
    session: &mut SessionRecord,
    index: usize,
) -> Submission {
    let item = QueueItem::new(format!("prompt-{index}"));
    let request = submission(&item, &format!("plak.command.{index}"));
    session.pending_queue.push(item);
    store.commit_submission(session, &request).unwrap();
    project_delivery(store, &session.id, &request.command_id, Delivery::Accepted);
    session.pending_queue.clear();
    store
        .discard_submission(session, &request.command_id, None)
        .unwrap();
    request
}

#[test]
fn exact_schema_wal_and_cas() {
    assert_eq!(
        SCHEMA.fingerprint(),
        "f16a62dc29b0309acbc65d3f681a9994a3f54f76c5f956377f3d7144db0fc104"
    );
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    assert_exact_schema(&paths);
    let mut session = SessionRecord::new(root.path().to_path_buf());
    store.commit(&mut session).unwrap();
    assert_eq!(session.store_revision, 1);
    let mut stale = session.clone();
    store.commit(&mut session).unwrap();
    assert_eq!(
        store.commit(&mut stale).unwrap_err().to_string(),
        "product session changed concurrently"
    );
    let c = Connection::open(paths.product_database()).unwrap();
    assert_eq!(
        c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
}

#[test]
fn target_selection_round_trips_native_and_openrouter() {
    use crate::product_config::{
        ProductCompositionSelection, ProductOpenRouterRoutingSelection, ProductProtocolSelection,
        ProductWireBinding,
    };

    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().join("workspace"));
    session.target_selection = ProductTargetSelection::OpenRouter {
        wire: ProductWireBinding::resolve(ProductProtocolSelection::Auto),
        routing: ProductOpenRouterRoutingSelection::GatewayManaged,
    };
    session.account = Some("openrouter".into());
    session.model = Some("fixture/openrouter-model".into());
    session.effort = Some("medium".into());
    session.composition = ProductCompositionSelection::Direct;
    let id = session.id.clone();
    store.commit(&mut session).unwrap();
    drop(store);

    let mut reopened = ProductStore::open(&paths).unwrap();
    let loaded = reopened.load(&id).unwrap();
    assert_eq!(loaded.target_selection, session.target_selection);
    assert_eq!(
        reopened
            .list_for_workspace(&session.workspace_root)
            .unwrap()[0]
            .target_selection,
        session.target_selection
    );
}

/// The root's frozen tools are written once: a save without them, or
/// with others, keeps what the first creation froze.
#[test]
fn frozen_root_tools_round_trip_and_are_written_once() {
    use plakous_hosted::binding::RootTemplateFact;
    use plakous_hosted::frozen_tools::FrozenRootTools;
    use plakous_hosted::tool_plan::TemplateToolSelection;

    let fact = |tools: &[&str]| RootTemplateFact {
        name: "root-agent".into(),
        account: "codex".into(),
        model: "m".into(),
        tools: TemplateToolSelection::parse(tools).unwrap(),
        tool_sections: std::collections::BTreeMap::from([(
            "tools.fetch_url".to_owned(),
            "[tools.fetch_url]\nmax_bytes = 4096\n".to_owned(),
        )]),
    };
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().join("workspace"));
    let id = session.id.clone();
    store.commit(&mut session).unwrap();
    assert_eq!(store.load(&id).unwrap().frozen_tools, None);

    let frozen = FrozenRootTools::new(Some(fact(&["fetch_url"]))).with_mcp_tools(Vec::new());
    session.frozen_tools = Some(frozen.clone());
    store.commit(&mut session).unwrap();
    session.frozen_tools = Some(FrozenRootTools::new(Some(fact(&["image_gen"]))));
    store.commit(&mut session).unwrap();
    session.frozen_tools = None;
    store.commit(&mut session).unwrap();
    drop(store);

    let mut reopened = ProductStore::open(&paths).unwrap();
    assert_eq!(reopened.load(&id).unwrap().frozen_tools, Some(frozen));
}

/// Product 36's node, virtual-tree profile and runner display copy: a new
/// record writes today's values (its own tree's root, no profile, the local
/// runner), they round trip through load and listing, and a malformed or
/// rewritten value refuses.
#[test]
fn a_record_writes_todays_node_profile_and_runner_and_refuses_malformed_ones() {
    use plakous_hosted::sessions::{RootProfile, RunnerDisplay, SessionNode};
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let workspace = root.path().join("workspace");
    let mut session = SessionRecord::new(workspace.clone());
    let id = session.id.clone();
    store.commit(&mut session).unwrap();
    let stored = |store: &ProductStore, id: &str| {
        store
            .connection
            .query_row(
                "SELECT node_tree_id,node_agent_id,root_profile,runner_display FROM sessions WHERE id=?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .unwrap()
    };
    assert_eq!(stored(&store, &id), (id.clone(), id.clone(), None, None));
    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.node, SessionNode::genesis(&id));
    assert_eq!((loaded.root_profile, loaded.runner), (None, None));

    let mut copy = SessionRecord::new(workspace.clone());
    copy.node = SessionNode::parse("tree-1", "c39").unwrap();
    copy.root_profile = Some(RootProfile {
        root_template: Some("lead".to_owned()),
        working_set: vec!["worker".to_owned()],
    });
    copy.runner = Some(RunnerDisplay {
        name: "buildbox".to_owned(),
        runner_id: "runner-1".to_owned(),
        workspace_root: workspace.clone(),
        workspace_cwd: workspace.join("crate"),
    });
    store.commit(&mut copy).unwrap();
    let loaded = store.load(&copy.id).unwrap();
    assert_eq!(
        (&loaded.node, &loaded.root_profile, &loaded.runner),
        (&copy.node, &copy.root_profile, &copy.runner)
    );
    let listed = store.list_for_workspace(&workspace).unwrap();
    let listed = listed.iter().find(|record| record.id == copy.id).unwrap();
    assert_eq!(
        (&listed.node, &listed.root_profile, &listed.runner),
        (&copy.node, &copy.root_profile, &copy.runner)
    );

    // The node is written once: a save that moved it is refused.
    let mut moved = store.load(&id).unwrap();
    moved.node = SessionNode::genesis("elsewhere");
    assert!(store.commit(&mut moved).is_err());
    let mut moved = store.load(&copy.id).unwrap();
    moved.root_profile = Some(RootProfile::default());
    assert!(store.commit(&mut moved).is_err());
    // A record without a profile gains it once, at its genesis.
    let mut unfrozen = store.load(&id).unwrap();
    unfrozen.root_profile = Some(RootProfile::default());
    store.commit(&mut unfrozen).unwrap();
    assert_eq!(
        store.load(&id).unwrap().root_profile,
        Some(RootProfile::default())
    );
    let mut moved = store.load(&copy.id).unwrap();
    moved.runner = None;
    assert!(store.commit(&mut moved).is_err());
    let mut invalid = SessionRecord::new(workspace.clone());
    invalid.root_profile = Some(RootProfile {
        root_template: None,
        working_set: vec!["worker".to_owned(), "worker".to_owned()],
    });
    assert!(store.commit(&mut invalid).is_err());

    for corrupt in [
        "UPDATE sessions SET node_agent_id='ro ot' WHERE id=?1",
        "UPDATE sessions SET root_profile='{\"root_template\":null}' WHERE id=?1",
        "UPDATE sessions SET runner_display='{\"name\":\"buildbox\"}' WHERE id=?1",
    ] {
        let mut scratch = SessionRecord::new(workspace.clone());
        store.commit(&mut scratch).unwrap();
        store.connection.execute(corrupt, [&scratch.id]).unwrap();
        assert!(store.load(&scratch.id).is_err(), "{corrupt}");
        store
            .connection
            .execute("DELETE FROM sessions WHERE id=?1", [&scratch.id])
            .unwrap();
    }
    assert!(store.load(&id).is_ok());
}

/// The resume picker's summary is the session's *opening* prompt, and its
/// age column is the record's own timestamps. A listing projects all three
/// even though `transcript` carries the most recent prompts instead.
#[test]
fn a_workspace_listing_projects_the_first_prompt_and_both_timestamps() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let workspace = root.path().join("workspace");

    let mut older = SessionRecord::new(workspace.clone());
    older.created_at_unix = 1_788_000_000;
    older.updated_at_unix = 1_788_100_000;
    store.commit(&mut older).unwrap();
    project_rows(
        &mut store,
        &older.id,
        &[
            TranscriptRow::user("the very first thing asked"),
            TranscriptRow::assistant("an answer"),
            TranscriptRow::user("the most recent thing asked"),
        ],
    );

    let mut newer = SessionRecord::new(workspace.clone());
    newer.created_at_unix = 1_788_200_000;
    newer.updated_at_unix = 1_788_300_000;
    store.commit(&mut newer).unwrap();
    project_rows(
        &mut store,
        &newer.id,
        &[TranscriptRow::assistant("nobody prompted this one")],
    );

    let listed = store.list_for_workspace(&workspace).unwrap();
    assert_eq!(
        listed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        [newer.id.as_str(), older.id.as_str()],
        "newest first"
    );
    assert_eq!(listed[0].created_at_unix, 1_788_200_000);
    assert_eq!(listed[0].updated_at_unix, 1_788_300_000);
    assert_eq!(
        listed[0].first_prompt, None,
        "a session with no user line projects no first prompt"
    );
    assert_eq!(listed[1].created_at_unix, 1_788_000_000);
    assert_eq!(listed[1].updated_at_unix, 1_788_100_000);
    assert_eq!(
        listed[1].first_prompt.as_deref(),
        Some("the very first thing asked"),
        "the projection is the opening prompt, not the latest one"
    );
    assert!(
        listed[1].transcript.iter().any(|line| line
            .text()
            .is_some_and(|text| text.contains("the most recent thing asked"))),
        "and the recent-prompt projection is untouched"
    );

    // Loading one record is not a listing: the projection stays absent
    // rather than being half-filled from a different query.
    assert_eq!(store.load(&older.id).unwrap().first_prompt, None);
}

/// A first prompt longer than the picker's bound is cut at the store, so a
/// pasted wall of text never reaches a one-line row.
#[test]
fn a_projected_first_prompt_is_bounded_to_the_pickers_column() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let workspace = root.path().join("workspace");
    let mut session = SessionRecord::new(workspace.clone());
    store.commit(&mut session).unwrap();
    project_rows(
        &mut store,
        &session.id,
        &[TranscriptRow::user("\u{e9}".repeat(500))],
    );

    let listed = store.list_for_workspace(&workspace).unwrap();
    let projected = listed[0].first_prompt.as_deref().unwrap();
    assert_eq!(
        projected.chars().count(),
        MAX_LIST_FIRST_PROMPT_CHARS,
        "the bound counts characters, not bytes"
    );
}

#[test]
fn child_wake_cancel_suppression_round_trips() {
    let dir = tempdir().unwrap();
    let paths = PlakPaths::explicit(dir.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(dir.path().to_owned());
    store.commit(&mut session).unwrap();
    store.write_host_controls(&session.id, true, None).unwrap();
    drop(store);
    let mut store = ProductStore::open(&paths).unwrap();
    let saved = store.load(&session.id).unwrap();
    assert!(saved.child_wake_suppressed);
}

#[test]
fn prompt_history_round_trips_across_store_restart_without_transcript_reconstruction() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.record_prompt("first".into());
    session.record_prompt("second".into());
    let id = session.id.clone();
    {
        let mut store = ProductStore::open(&paths).unwrap();
        store.commit(&mut session).unwrap();
        project_rows(&mut store, &id, &[TranscriptRow::user("transcript-only")]);
    }
    let mut restarted = ProductStore::open(&paths).unwrap();
    let loaded = restarted.load(&id).unwrap();
    assert_eq!(loaded.prompt_history, ["first", "second"]);
    assert_eq!(
        loaded.transcript,
        vec![TranscriptRow::user("transcript-only")]
    );
}

#[test]
fn prompt_history_append_and_front_prune_touch_constant_rows() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    for index in 0..sessions::MAX_PROMPT_HISTORY {
        session.record_prompt(format!("prompt-{index}"));
    }
    store.commit(&mut session).unwrap();
    store.connection.execute_batch(
        "CREATE TEMP TABLE prompt_work(inserted INTEGER NOT NULL,deleted INTEGER NOT NULL);
             INSERT INTO prompt_work VALUES(0,0);
             CREATE TEMP TRIGGER count_prompt_insert AFTER INSERT ON prompt_history BEGIN UPDATE prompt_work SET inserted=inserted+1; END;
             CREATE TEMP TRIGGER count_prompt_delete AFTER DELETE ON prompt_history BEGIN UPDATE prompt_work SET deleted=deleted+1; END;",
    ).unwrap();
    session.record_prompt("prompt-500".into());
    store.commit(&mut session).unwrap();
    assert_eq!(
        store
            .connection
            .query_row("SELECT inserted,deleted FROM prompt_work", [], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap(),
        (1, 1)
    );
}
/// The admission steps are shared with the tree store; the product's
/// refusals keep their exact text and its fingerprint keeps its algorithm.
#[test]
fn product_admission_refusals_and_fingerprint_are_exact() {
    assert_eq!(
        SCHEMA.fingerprint(),
        digest_parts(
            &SCHEMA_OBJECTS
                .iter()
                .flat_map(|(name, sql)| [*name, *sql])
                .collect::<Vec<_>>()
        )
    );
    let refusal = |damage: &str| {
        let root = tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("p"));
        paths.ensure_layout().unwrap();
        if damage != "unversioned" {
            drop(ProductStore::open(&paths).unwrap());
        }
        let connection = Connection::open(paths.product_database()).unwrap();
        let damage_sql = match damage {
            "unversioned" | "shape" => "CREATE TABLE stray(x)",
            "fingerprint" => "UPDATE product_schema SET fingerprint='drift'",
            "missing" => "DELETE FROM product_schema",
            _ => "PRAGMA user_version=99",
        };
        connection.execute_batch(damage_sql).unwrap();
        drop(connection);
        let error = ProductStore::open(&paths).unwrap_err();
        (error.kind(), error.to_string())
    };
    assert_eq!(
        refusal("unversioned"),
        (
            ProductStoreOpenErrorKind::Unavailable,
            "unversioned product store contains unrecognized state; archive product.sqlite3 and runtime.sqlite3 together, then retry".to_owned()
        )
    );
    assert_eq!(
        refusal("shape"),
        (
            ProductStoreOpenErrorKind::Unavailable,
            "product store schema shape does not match its exact revision; current-schema product state is not authoritative".to_owned()
        )
    );
    assert_eq!(
        refusal("fingerprint"),
        (
            ProductStoreOpenErrorKind::Unavailable,
            "product store schema fingerprint does not match its exact revision; current-schema product state is not authoritative".to_owned()
        )
    );
    assert_eq!(
        refusal("missing"),
        (
            ProductStoreOpenErrorKind::Unavailable,
            "product store schema fingerprint is missing; current-schema product state is not authoritative".to_owned()
        )
    );
    assert_eq!(
        refusal("version"),
        (
            ProductStoreOpenErrorKind::UnsupportedPersistence,
            format!(
                "product schema 99, expected {SCHEMA_VERSION}: this product.sqlite3 was created by an incompatible plak build; move it aside to start fresh"
            )
        )
    );
}

#[test]
fn unknown_schema_is_refused() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    paths.ensure_layout().unwrap();
    let c = Connection::open(paths.product_database()).unwrap();
    c.pragma_update(None, "user_version", 99).unwrap();
    drop(c);
    let error = ProductStore::open(&paths).unwrap_err();
    assert_eq!(
        error.kind(),
        ProductStoreOpenErrorKind::UnsupportedPersistence
    );
    assert!(
        error
            .to_string()
            .contains("created by an incompatible plak build")
    );
}

#[test]
fn current_schema_rejects_previous_product_store_without_modification() {
    // A product schema change moves the state directory; a store at
    // another version in this directory is refused with its rows intact.
    assert_eq!(SCHEMA_VERSION, 38);
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    paths.ensure_layout().unwrap();
    {
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        store.commit(&mut session).unwrap();
    }
    let connection = Connection::open(paths.product_database()).unwrap();
    connection.pragma_update(None, "user_version", 37).unwrap();
    drop(connection);

    let error = ProductStore::open(&paths).unwrap_err();
    assert_eq!(
        error.kind(),
        ProductStoreOpenErrorKind::UnsupportedPersistence
    );
    assert!(
        error.to_string().contains("product schema 37, expected 38"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("created by an incompatible plak build"),
        "{error}"
    );
    // Fail-closed means the rows are still there, untouched.
    let connection = Connection::open(paths.product_database()).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn workspace_listing_uses_two_set_queries_and_preserves_bounded_previews() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let workspace = root.path().join("workspace");
    let mut store = ProductStore::open(&paths).unwrap();

    let mut prompted = SessionRecord::new(workspace.clone());
    store.commit(&mut prompted).unwrap();
    project_rows(
        &mut store,
        &prompted.id,
        &[
            TranscriptRow::assistant("older answer"),
            TranscriptRow::user("first prompt"),
            TranscriptRow::assistant("newer answer"),
            TranscriptRow::user("second prompt"),
        ],
    );
    let mut fallback = SessionRecord::new(workspace.clone());
    store.commit(&mut fallback).unwrap();
    project_rows(
        &mut store,
        &fallback.id,
        &[
            TranscriptRow::error(NoticeScope::Turn, "ignored"),
            TranscriptRow::assistant("first usable line"),
            TranscriptRow::assistant("later line"),
        ],
    );
    let mut empty = SessionRecord::new(workspace.clone());
    store.commit(&mut empty).unwrap();

    let (listed, query_count) = store
        .list_for_workspace_with_query_count(&workspace, true)
        .unwrap();
    assert_eq!(
        query_count, 2,
        "picker latency must not grow by SQL round trips"
    );
    assert_eq!(listed.len(), 3);
    let prompted = listed.iter().find(|row| row.id == prompted.id).unwrap();
    assert_eq!(
        prompted.transcript,
        vec![
            TranscriptRow::user("first prompt"),
            TranscriptRow::user("second prompt")
        ]
    );
    let fallback = listed.iter().find(|row| row.id == fallback.id).unwrap();
    assert_eq!(
        fallback.transcript,
        vec![TranscriptRow::assistant("first usable line")]
    );
    let empty = listed.iter().find(|row| row.id == empty.id).unwrap();
    assert!(empty.transcript.is_empty());
}

#[test]
fn corrupt_database_is_unavailable_not_unsupported_persistence() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    paths.ensure_layout().unwrap();
    std::fs::write(paths.product_database(), b"not a sqlite database").unwrap();

    let error = ProductStore::open(&paths).unwrap_err();

    assert_eq!(error.kind(), ProductStoreOpenErrorKind::Unavailable);
}

#[cfg(unix)]
#[test]
fn symlinked_product_database_is_refused_without_touching_target() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    paths.ensure_layout().unwrap();
    let external = root.path().join("external.sqlite3");
    std::fs::write(&external, b"external sentinel").unwrap();
    symlink(&external, paths.product_database()).unwrap();

    let error = ProductStore::open(&paths).unwrap_err();

    assert_eq!(error.kind(), ProductStoreOpenErrorKind::Unavailable);
    assert_eq!(std::fs::read(external).unwrap(), b"external sentinel");
}

#[test]
fn global_registry_terminal_domain_and_exact_transition_are_closed() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    for (kind, state) in [
        ("create", "rejected"),
        ("delivery", "terminal_no_change"),
        ("workflow", "rejected"),
    ] {
        assert!(store.connection.execute("INSERT INTO identity_bindings(identity,kind,owner_session_id,binding_digest,terminal_disposition) VALUES(?1,?2,'owner','digest',?3)",params![format!("bad:{kind}"),kind,state]).is_err());
    }
    let tx = store.connection.transaction().unwrap();
    bind_identity(&tx, "delivery", "operation", "owner", "digest", None, None).unwrap();
    bind_identity(
        &tx,
        "delivery",
        "operation",
        "owner",
        "digest",
        Some("product_committed"),
        None,
    )
    .unwrap();
    bind_identity(
        &tx,
        "delivery",
        "operation",
        "owner",
        "digest",
        Some("product_committed"),
        None,
    )
    .unwrap();
    tx.commit().unwrap();
    let tx = store.connection.transaction().unwrap();
    assert!(bind_identity(&tx, "delivery", "operation", "owner", "digest", None, None).is_err());
    drop(tx);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT terminal_disposition FROM identity_bindings WHERE identity='operation'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "product_committed"
    );
}

#[test]
fn unrecorded_refusal_releases_submission_but_preserves_exact_retry_binding() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_owned());
    let item = QueueItem::new("retry");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "retry-command");
    for _ in 0..2 {
        store.commit_submission(&mut session, &request).unwrap();
        store
            .discard_submission(&mut session, &request.command_id, None)
            .unwrap();
        session = store.load(&session.id).unwrap();
        assert_eq!(
            store.load_intents(&session.id).unwrap().submissions,
            [] as [Submission; 0]
        );
        assert_eq!(session.pending_queue, vec![item.clone()]);
        let bindings: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM identity_bindings WHERE identity=?1",
                [request.binding_identity()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bindings, 1);
        let conflict = Submission {
            canonical_command: "changed".into(),
            ..request.clone()
        };
        assert!(store.commit_submission(&mut session, &conflict).is_err());
    }
}

#[test]
fn result_mail_command_roundtrips_and_is_part_of_immutable_submission_identity() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_owned());
    let item = QueueItem::new("continue");
    session.pending_queue.push(item.clone());
    store.commit(&mut session).unwrap();
    let mut request = submission(&item, "result-command");
    request.canonical_command = "continue with exact result m1".into();
    let mut competing = session.clone();
    let changed = Submission {
        canonical_command: "continue with exact result m2".into(),
        ..request.clone()
    };
    let mut second_handle = ProductStore::open(&paths).unwrap();
    store.commit_submission(&mut session, &request).unwrap();
    assert!(
        second_handle
            .commit_submission(&mut competing, &changed)
            .is_err()
    );
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request.clone()]
    );
    let mut loaded = store.load(&session.id).unwrap();
    assert!(store.commit_submission(&mut loaded, &changed).is_err());
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request]
    );
}

#[test]
fn reading_settled_submissions_does_not_change_the_next_commit_writes() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let item = QueueItem::new("command body stays immutable");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "settled-command");
    store.commit_submission(&mut session, &request).unwrap();
    project_delivery(
        &mut store,
        &session.id,
        &request.command_id,
        Delivery::Accepted,
    );
    store.connection.execute_batch("CREATE TEMP TABLE submission_writes(command_id TEXT NOT NULL); CREATE TEMP TRIGGER observe_submission_update AFTER UPDATE ON submissions BEGIN INSERT INTO submission_writes VALUES(NEW.command_id); END;").unwrap();
    assert_eq!(
        store.load(&session.id).unwrap().pending_queue,
        [] as [QueueItem; 0]
    );
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request]
    );
    store.commit(&mut session).unwrap();
    let writes: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM submission_writes", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        writes, 0,
        "a read changed the next intent commit's write shape"
    );
}

#[test]
fn pending_rejection_survives_submission_settlement_and_reopen() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let item = QueueItem::new("rejected");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "rejected-command");
    store.commit_submission(&mut session, &request).unwrap();
    project_delivery(
        &mut store,
        &session.id,
        &request.command_id,
        Delivery::Rejected {
            error: "refused".into(),
        },
    );
    // The pending rejected revision must survive unrelated settled commands.
    for index in 0..129 {
        let next = QueueItem::new(format!("prompt-{index}"));
        let next_request = submission(&next, &format!("command-{index}"));
        session.pending_queue.push(next.clone());
        store
            .commit_submission(&mut session, &next_request)
            .unwrap();
        project_delivery(
            &mut store,
            &session.id,
            &next_request.command_id,
            Delivery::Accepted,
        );
        session.pending_queue.retain(|item| item.id != next.id);
        store
            .discard_submission(&mut session, &next_request.command_id, None)
            .unwrap();
    }
    drop(store);
    let mut store = ProductStore::open(&paths).unwrap();
    let loaded = store.load(&session.id).unwrap();
    assert_eq!(loaded.pending_queue, vec![item.clone()]);
    assert!(loaded.queue_commands.is_rejected(&item));
    assert!(!loaded.queue_commands.is_locked(&item.id));
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request]
    );
}

#[test]
fn lifetime_submission_4097_removes_detail_but_preserves_exact_retry_binding() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let first = append_settled_submission(&mut store, &mut session, 0);
    let tx = store.connection.transaction().unwrap();
    for index in 1..4096 {
        let request = submission(
            &QueueItem::new(format!("prompt-{index}")),
            &format!("command-{index}"),
        );
        bind_identity(
            &tx,
            "command",
            &request.binding_identity(),
            &session.id,
            &request.binding_digest(&session.id),
            None,
            None,
        )
        .unwrap();
    }
    tx.commit().unwrap();
    append_settled_submission(&mut store, &mut session, 4096);
    let detail: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM submissions", [], |row| row.get(0))
        .unwrap();
    let bindings: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM identity_bindings WHERE kind='command'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(detail, 0);
    assert_eq!(bindings, 4097);
    let conflict = Submission {
        canonical_command: "changed prompt-0".into(),
        ..first.clone()
    };
    assert!(store.commit_submission(&mut session, &conflict).is_err());
    store.write_submission(&session.id, &first).unwrap();
    store.write_submission(&session.id, &first).unwrap();
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![first]
    );
}

#[test]
fn submission_withdrawal_failure_rolls_back_registry_and_parent() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let item = QueueItem::new("rollback command");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "rollback-command");
    store.commit_submission(&mut session, &request).unwrap();
    let before = store.load(&session.id).unwrap();
    let revision = session.store_revision;
    store.connection.execute_batch("CREATE TEMP TRIGGER fail_submission_withdrawal BEFORE DELETE ON submissions BEGIN SELECT RAISE(ABORT,'cut'); END;").unwrap();
    session.pending_queue.clear();
    assert!(
        store
            .discard_submission(&mut session, &request.command_id, None)
            .is_err()
    );
    assert_eq!(session.store_revision, revision);
    assert_eq!(store.load(&session.id).unwrap(), before);
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request.clone()]
    );
    let disposition: Option<String> = store
        .connection
        .query_row(
            "SELECT terminal_disposition FROM identity_bindings WHERE identity=?1",
            [&item.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(disposition.is_none());
    assert!(
        store
            .write_submission(
                &session.id,
                &Submission {
                    canonical_command: "changed".into(),
                    ..request
                }
            )
            .is_err()
    );
}

#[test]
fn unresolved_submission_survives_unrelated_settlements() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    for index in 0..129 {
        append_settled_submission(&mut store, &mut session, index);
    }
    let item = QueueItem::new("still unresolved");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "unresolved-command");
    store.commit_submission(&mut session, &request).unwrap();
    store.commit(&mut session).unwrap();
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request]
    );
    assert!(
        store
            .load(&session.id)
            .unwrap()
            .queue_commands
            .is_locked(&item.id)
    );
}

#[test]
fn user_queue_delete_seals_tombstone_and_forbids_resurrection() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let item = QueueItem::new("delete me");
    session.pending_queue.push(item.clone());
    store.commit(&mut session).unwrap();
    session.pending_queue.clear();
    store.commit(&mut session).unwrap();
    assert_eq!(
        store.load(&session.id).unwrap().pending_queue,
        [] as [QueueItem; 0]
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT terminal_disposition FROM identity_bindings WHERE identity=?1",
                [&item.id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "deleted"
    );
    session.pending_queue.push(item);
    assert!(store.commit(&mut session).is_err());
}

#[test]
fn accepted_submission_consumes_only_its_revision_before_queue_cleanup() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let item = QueueItem::new("consume me");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "consume-command");
    store.commit_submission(&mut session, &request).unwrap();
    project_delivery(
        &mut store,
        &session.id,
        &request.command_id,
        Delivery::Accepted,
    );
    assert_eq!(
        store.load(&session.id).unwrap().pending_queue,
        [] as [QueueItem; 0]
    );
    let retained: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM queue_items WHERE session_id=?1",
            [&session.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, 1, "projection does not rewrite queue intents");
    session.pending_queue.clear();
    store.commit(&mut session).unwrap();
    session.pending_queue.push(item);
    assert!(store.commit(&mut session).is_err());
}

#[test]
fn projected_queue_reads_do_not_suppress_the_consuming_intent_write() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let item = QueueItem::new("consume me");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "projected-consume");
    store.commit_submission(&mut session, &request).unwrap();
    project_delivery(
        &mut store,
        &session.id,
        &request.command_id,
        Delivery::Accepted,
    );
    assert_eq!(
        store.load(&session.id).unwrap().pending_queue,
        [] as [QueueItem; 0]
    );
    session.pending_queue.clear();
    store.commit(&mut session).unwrap();
    let remaining: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM queue_items WHERE session_id=?1",
            [&session.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
}

#[test]
fn queue_edit_requires_exact_replay_or_atomic_revision_increment() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.pending_queue.push(QueueItem::new("one"));
    store.commit(&mut session).unwrap();
    session.pending_queue[0].text = "rewrite".into();
    assert!(store.commit(&mut session).is_err());
    session.pending_queue[0].revision = 2;
    store.commit(&mut session).unwrap();
    assert_eq!(
        store.load(&session.id).unwrap().pending_queue[0].text,
        "rewrite"
    );
}

#[test]
fn queued_attachments_round_trip_survive_edit_and_leave_with_their_item() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let image = |name: &str, width| plakous_host_protocol::DraftImage {
        name: name.to_owned(),
        media_type: "image/png".to_owned(),
        bytes: plakous_hosted::attachments::png(width, 3, 100),
        source_dimensions: None,
    };
    let mut with_images = QueueItem::new("look at these");
    with_images.images = vec![image("a.png", 2), image("b.png", 5)].into();
    session.pending_queue.push(with_images.clone());
    session.pending_queue.push(QueueItem::new("text only"));
    store.commit(&mut session).unwrap();
    let loaded = store.load(&session.id).unwrap();
    assert_eq!(loaded.pending_queue, session.pending_queue);
    assert_eq!(loaded.pending_queue[0].images.len(), 2);
    assert_eq!(loaded.pending_queue[0].images[1].name, "b.png");
    assert!(loaded.pending_queue[1].images.is_empty());

    // An edit replaces the text; the images stay with the item.
    session.pending_queue[0].text = "look again".into();
    session.pending_queue[0].revision = 2;
    store.commit(&mut session).unwrap();
    let edited = store.load(&session.id).unwrap();
    assert_eq!(edited.pending_queue[0].text, "look again");
    assert_eq!(edited.pending_queue[0].images, with_images.images);

    // Images alone are a queued message; text alone or nothing is not.
    session.pending_queue[0].text = String::new();
    session.pending_queue[0].revision = 3;
    store.commit(&mut session).unwrap();
    assert_eq!(store.load(&session.id).unwrap().pending_queue[0].text, "");
    session.pending_queue[1].text = String::new();
    assert!(store.commit(&mut session).is_err());
    session.pending_queue[1].text = "text only".into();

    let attachments = |store: &ProductStore| -> i64 {
        store
            .connection
            .query_row("SELECT COUNT(*) FROM queue_attachments", [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(attachments(&store), 2);
    session.pending_queue.remove(0);
    store.commit(&mut session).unwrap();
    assert_eq!(attachments(&store), 0);
    assert_eq!(store.load(&session.id).unwrap().pending_queue.len(), 1);
}

#[test]
fn snapshot_load_rejects_submission_registry_detail_contradiction() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let item = QueueItem::new("bound prompt");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "bound-command");
    store.commit_submission(&mut session, &request).unwrap();
    store
        .connection
        .execute(
            "UPDATE identity_bindings SET binding_digest='contradiction' WHERE identity=?1",
            [request.binding_identity()],
        )
        .unwrap();
    assert!(store.load(&session.id).is_err());
    assert!(store.load_intents(&session.id).is_err());
    assert!(store.commit_submission(&mut session, &request).is_err());
}

fn bound_submission(
    root: &Path,
    command: &str,
) -> (PlakPaths, ProductStore, SessionRecord, Submission) {
    let paths = PlakPaths::explicit(root.join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.to_path_buf());
    let item = QueueItem::new("bound prompt");
    session.pending_queue.push(item.clone());
    let request = submission(&item, command);
    store.commit_submission(&mut session, &request).unwrap();
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request.clone()]
    );
    (paths, store, session, request)
}

/// Writes a submission row without its identity binding.
fn insert_out_of_band(store: &ProductStore, session_id: &str, request: &Submission) {
    store
        .connection
        .execute(
            "INSERT INTO submissions(session_id,command_id,kind,queue_item_id,queue_revision,target_turn_id,canonical_command,submitted_at_ms,anchor) VALUES(?1,?2,?3,?4,?5,NULL,?6,?7,0)",
            params![
                session_id,
                request.command_id,
                request.kind.as_str(),
                request.queue_item_id,
                to_i64(request.queue_item_revision).unwrap(),
                request.canonical_command,
                to_i64(request.submitted_at_ms).unwrap(),
            ],
        )
        .unwrap();
}

fn contradict_binding(store: &ProductStore, request: &Submission) {
    store
        .connection
        .execute(
            "UPDATE identity_bindings SET binding_digest='contradiction' WHERE identity=?1",
            [request.binding_identity()],
        )
        .unwrap();
}

/// Spec 02 §2.6: a store checks each submission against its binding once.
/// A row that differs from the one it verified is checked again and
/// refused; a contradicted binding is refused by the next process's first
/// load.
#[test]
fn a_submission_is_verified_once_per_store_and_tampering_is_still_refused() {
    let root = tempdir().unwrap();
    let (paths, mut store, session, request) = bound_submission(root.path(), "verified");
    store
        .connection
        .execute(
            "UPDATE submissions SET canonical_command='tampered' WHERE command_id=?1",
            [&request.command_id],
        )
        .unwrap();
    assert!(store.load_intents(&session.id).is_err());
    assert!(store.load(&session.id).is_err());
    store
        .connection
        .execute(
            "UPDATE submissions SET canonical_command=?2 WHERE command_id=?1",
            params![request.command_id, request.canonical_command],
        )
        .unwrap();
    contradict_binding(&store, &request);
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request.clone()],
        "this store already verified the unchanged row"
    );
    drop(store);
    let mut store = ProductStore::open(&paths).unwrap();
    assert!(store.load_intents(&session.id).is_err());
    assert!(store.load(&session.id).is_err());
}

/// A submission verified inside a transaction that rolls back is not
/// remembered: the same row reappearing without its binding is refused.
#[test]
fn a_rolled_back_submission_is_not_remembered() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let item = QueueItem::new("bound prompt");
    session.pending_queue.push(item.clone());
    store.commit(&mut session).unwrap();
    let request = submission(&item, "rolled-back");
    store
        .connection
        .execute_batch("CREATE TEMP TRIGGER fail_pause BEFORE INSERT ON host_intents BEGIN SELECT RAISE(ABORT,'cut'); END;")
        .unwrap();
    let cause = plakous_hosted::sessions::projector::HostPauseCause::UnrecordedRefusal {
        reason: "cut".to_owned(),
    };
    assert!(
        store
            .commit_with_submission(&mut session, Some(&request), None, Some(cause))
            .is_err()
    );
    store
        .connection
        .execute_batch("DROP TRIGGER fail_pause;")
        .unwrap();
    let bindings: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM identity_bindings WHERE identity=?1",
            [request.binding_identity()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bindings, 0, "the binding rolled back with the row");
    insert_out_of_band(&store, &session.id, &request);
    assert!(store.load_intents(&session.id).is_err());
}

/// A discarded submission is forgotten: the same row reappearing under a
/// contradicted binding is checked again and refused.
#[test]
fn discarding_a_submission_forgets_its_verification() {
    let root = tempdir().unwrap();
    let (_paths, mut store, mut session, request) = bound_submission(root.path(), "discarded");
    store
        .discard_submission(&mut session, &request.command_id, None)
        .unwrap();
    contradict_binding(&store, &request);
    insert_out_of_band(&store, &session.id, &request);
    assert!(store.load_intents(&session.id).is_err());
}

/// A deleted session's submissions are forgotten: the same rows restored
/// under a contradicted binding are checked again and refused.
#[test]
fn deleting_a_session_forgets_its_verified_submissions() {
    let root = tempdir().unwrap();
    let (_paths, mut store, session, request) = bound_submission(root.path(), "deleted");
    store
        .connection
        .execute_batch(&format!(
            "CREATE TEMP TABLE saved_session AS SELECT * FROM sessions WHERE id='{0}';
                 CREATE TEMP TABLE saved_submissions AS SELECT * FROM submissions WHERE session_id='{0}';",
            session.id
        ))
        .unwrap();
    store
        .delete_session(&session.id, session.store_revision)
        .unwrap();
    contradict_binding(&store, &request);
    store
        .connection
        .execute_batch(
            "INSERT INTO sessions SELECT * FROM saved_session;
                 INSERT INTO submissions SELECT * FROM saved_submissions;",
        )
        .unwrap();
    assert!(store.load_intents(&session.id).is_err());
}

#[test]
fn every_intent_commit_restores_full_durability() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let synchronous = |store: &ProductStore| -> i64 {
        store
            .connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap()
    };
    let mut session = SessionRecord::new(root.path().to_path_buf());
    store.commit(&mut session).unwrap();
    assert_eq!(synchronous(&store), 2, "authority commits are FULL");
    project_rows(&mut store, &session.id, &[TranscriptRow::user("go")]);
    assert_eq!(synchronous(&store), 1, "projection commits are NORMAL");
    session.pending_queue.push(QueueItem::new("next"));
    store.commit(&mut session).unwrap();
    assert_eq!(synchronous(&store), 2);
    project_rows(&mut store, &session.id, &[TranscriptRow::assistant("ok")]);
    let item = session.pending_queue[0].clone();
    let request = submission(&item, "cmd-sync");
    store.commit_submission(&mut session, &request).unwrap();
    assert_eq!(synchronous(&store), 2, "a submission commit is FULL");
}

fn locator(branch: u8, checkpoint: u8) -> SessionLocator {
    format!(
        "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-{branch:012}/00000000-0000-0000-0000-{checkpoint:012}"
    ).parse().unwrap()
}

fn other_runtime_locator(checkpoint: u8) -> SessionLocator {
    format!(
        "00000000-0000-0000-0000-000000000009/00000000-0000-0000-0000-000000000008/00000000-0000-0000-0000-{checkpoint:012}"
    ).parse().unwrap()
}

#[test]
fn construction_binding_is_write_once() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_owned());
    store.commit(&mut session).unwrap();
    session.bind_plakous_locator(locator(2, 3)).unwrap();
    store.commit(&mut session).unwrap();
    store.commit(&mut session).unwrap();
    session.bind_plakous_locator(locator(2, 4)).unwrap();
    store.commit(&mut session).unwrap();
    assert_eq!(
        store.load(&session.id).unwrap().plakous_locator,
        Some(locator(2, 3))
    );
}

#[test]
fn view_head_wins_without_rewriting_construction_binding() {
    use plakous::sessions::feed::FeedLocator;

    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut session = SessionRecord::new(root.path().to_owned());
    session.bind_plakous_locator(locator(2, 3)).unwrap();
    store.commit(&mut session).unwrap();
    let empty = SessionViews::empty();
    let mut projected = empty.clone();
    let head = locator(2, 4);
    projected.state.runtime_session_id = Some(head.session_id());
    projected.state.head = Some(FeedLocator {
        session_id: head.session_id(),
        branch_id: head.branch_id(),
        checkpoint_id: head.checkpoint_id(),
    });
    store
        .commit_projection_views(
            &session.id,
            &ViewWrites::between(&empty, &projected),
            &projected.state,
        )
        .unwrap();
    let mut loaded = store.load(&session.id).unwrap();
    assert_eq!(loaded.plakous_locator, Some(head));
    assert_eq!(
        store.session_locators().unwrap(),
        vec![(session.id.clone(), Some(head))]
    );
    assert_eq!(
        store.list_for_workspace(&session.workspace_root).unwrap()[0].plakous_locator,
        Some(head)
    );
    loaded.title = Some("metadata at the projected head".to_owned());
    store.commit(&mut loaded).unwrap();
    store.delete_views(&session.id).unwrap();
    assert_eq!(
        store.load(&session.id).unwrap().plakous_locator,
        Some(locator(2, 3))
    );
}

#[test]
fn deletion_dependents_are_continuations_and_runtime_sharers_and_deletion_is_exact() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut source = SessionRecord::new(root.path().to_path_buf());
    source.bind_plakous_locator(locator(2, 3)).unwrap();
    store.commit(&mut source).unwrap();
    let runtime = locator(2, 3).session_id();
    assert_eq!(
        store.session_dependents(&source.id, runtime).unwrap(),
        [] as [String; 0]
    );

    // A fork's head is another branch of the same runtime session.
    let mut fork = SessionRecord::new(root.path().to_path_buf());
    fork.bind_plakous_locator(locator(4, 5)).unwrap();
    store.commit(&mut fork).unwrap();
    // A rebind or clear target continues from its source.
    let mut continuation = SessionRecord::new(root.path().to_path_buf());
    continuation.source_product_session_id = Some(source.id.clone());
    continuation.source_workflow_operation_id = Some("plak:new:dependents-test".to_owned());
    continuation
        .bind_plakous_locator(other_runtime_locator(6))
        .unwrap();
    store.commit(&mut continuation).unwrap();
    let mut unrelated = SessionRecord::new(root.path().to_path_buf());
    unrelated
        .bind_plakous_locator(other_runtime_locator(7))
        .unwrap();
    // A carried-forward copy names its source but holds its own history.
    unrelated.link_carried_forward(&source.id, "previous-build");
    store.commit(&mut unrelated).unwrap();

    let mut dependents = store.session_dependents(&source.id, runtime).unwrap();
    dependents.sort();
    let mut expected = vec![fork.id.clone(), continuation.id.clone()];
    expected.sort();
    assert_eq!(dependents, expected);
    assert_eq!(
        store
            .session_dependents(&continuation.id, other_runtime_locator(6).session_id())
            .unwrap(),
        vec![unrelated.id.clone()]
    );

    // Deletion is exact to the checked revision and takes the owned rows.
    let stale = unrelated.store_revision;
    unrelated.record_notice(TranscriptRow::warning(NoticeScope::Tree, "moved"));
    store.commit(&mut unrelated).unwrap();
    assert!(store.delete_session(&unrelated.id, stale).is_err());
    store
        .delete_session(&unrelated.id, unrelated.store_revision)
        .unwrap();
    assert!(store.load(&unrelated.id).is_err());
    let orphaned: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM operator_notices WHERE session_id = ?1",
            [&unrelated.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphaned, 0);
}

#[test]
fn clear_publication_atomically_binds_distinct_workflow_and_create_identities() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut source = SessionRecord::new(root.path().to_path_buf());
    source.bind_plakous_locator(locator(2, 3)).unwrap();
    store.commit(&mut source).unwrap();
    let operation = "plak:new:atomic-test";
    let request = plakous_hosted::sessions::projector::WorkflowRequest {
        operation_id: operation.to_owned(),
        source_locator: locator(2, 3),
        kind: SessionWorkflowKind::New,
    };
    store
        .submit_workflow_request(&mut source, &request)
        .unwrap();
    let published = store.load(&request.successor_product_id()).unwrap();
    assert_eq!(published.create_operation_id, format!("{operation}:create"));
    assert!(published.transcript.is_empty());

    assert_eq!(
        store.load_intents(&source.id).unwrap().workflow_requests,
        vec![request]
    );
}
