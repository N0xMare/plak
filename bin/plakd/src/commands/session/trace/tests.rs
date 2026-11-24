use super::*;
use plakous::model::{
    Actor, CallId, ContentBlock, ConversationItem, DataClassification, EntryId, HistorySegment,
    HistorySegmentKind, ImageBlock, MediaSource, ModelId, ProtocolId, Provenance, ReasoningBlock,
    ReasoningVisibility, ReplayEnvelope, ReplayKindId, ReplayScope, TextBlock, ToolCallBlock,
    ToolCallState, ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind,
    ToolResultPart,
};
use plakous::sessions::{
    CreateSessionOutcome, CreateSessionRequest, PortableSessionHistory, SessionBackend,
    SessionOperationId,
};
use plakous_hosted::agent_tree::{AgentCommand, AgentCommandKind, TreeStore as _};
use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest};
use plakous_offline_crash_harness::{
    BoundaryController, protocol::CrashBoundary, text_runtime_binding,
};
use serde_json::Value;
use std::fs;

fn entry(n: u128, actor: Actor, origin: Provenance, blocks: Vec<ContentBlock>) -> ConversationItem {
    ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(n)).unwrap(),
        actor,
        blocks,
        origin,
    )
    .unwrap()
}

fn history() -> Vec<HistorySegment> {
    let call = CallId::try_from(uuid::Uuid::from_u128(9)).unwrap();
    let owner = ToolExecutionOwner::Local;
    let correlation = ToolCorrelation::default();
    let replay = ReplayEnvelope::new(
        ProtocolId::new("synthetic.protocol").unwrap(),
        None,
        ReplayScope::Protocol,
        ReplayKindId::new("synthetic.replay").unwrap(),
        1,
        DataClassification::Sensitive,
        serde_json::value::RawValue::from_string(r#"{"opaque":"synthetic-replay-body"}"#.into())
            .unwrap(),
    )
    .unwrap();
    vec![
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![entry(
                1,
                Actor::User,
                Provenance::UserInput,
                vec![
                    ContentBlock::Text(TextBlock::new("Inspect this input.", None).unwrap()),
                    ContentBlock::Image(
                        ImageBlock::new("image/png", MediaSource::bytes(vec![1, 2, 3]).unwrap())
                            .unwrap(),
                    ),
                ],
            )],
        )
        .unwrap(),
        HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            vec![
                entry(
                    2,
                    Actor::Assistant,
                    Provenance::ModelOutput,
                    vec![
                        ContentBlock::Reasoning(
                            ReasoningBlock::new(
                                ReasoningVisibility::Summary,
                                Some("Recorded reasoning.".to_owned()),
                                Some(replay),
                            )
                            .unwrap(),
                        ),
                        ContentBlock::ToolCall(
                            ToolCallBlock::new(
                                call,
                                ToolName::new("inspect").unwrap(),
                                owner.clone(),
                                ToolCallState::Requested,
                                correlation.clone(),
                                r#"{ "Authorization": "synthetic-header-value", "q": "input" }"#,
                            )
                            .unwrap(),
                        ),
                    ],
                ),
                entry(
                    3,
                    Actor::Tool,
                    Provenance::ToolExecution,
                    vec![ContentBlock::ToolResult(
                        ToolResultBlock::new(
                            call,
                            owner,
                            ToolResultKind::Success,
                            vec![
                                ToolResultPart::Text(TextBlock::new("First part.", None).unwrap()),
                                ToolResultPart::Text(TextBlock::new("Second part.", None).unwrap()),
                            ],
                            correlation,
                        )
                        .unwrap(),
                    )],
                ),
            ],
        )
        .unwrap(),
    ]
}

fn create_quiescent(paths: &PlakPaths, operation: &str) -> SessionLocator {
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let binding = text_runtime_binding(
        paths.home(),
        BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            paths.home().join("evidence"),
        )
        .unwrap(),
        "synthetic",
        "synthetic",
    )
    .unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let CreateSessionOutcome::Created { session, .. } = executor
        .block_on(
            sessions.create(
                CreateSessionRequest::new(SessionOperationId::new(operation).unwrap(), binding)
                    .with_history(PortableSessionHistory::new(
                        history(),
                        "unrelated-account".into(),
                        ModelId::new("unrelated-model").unwrap(),
                    )),
            ),
        )
        .unwrap()
        .unwrap()
    else {
        panic!("fixture needs a new runtime session");
    };
    executor.block_on(session.release()).unwrap().unwrap()
}

#[test]
fn captures_one_settled_child_while_root_and_sibling_are_busy() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    let paths = paths.for_workspace(paths.home());
    paths.ensure_layout().unwrap();
    let mut record = SessionRecord::new(repo.path().to_owned());
    let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
        &record.id,
        repo.path().to_owned(),
    );
    let mut request = SpawnAgentRequest::test_review();
    request.output_schema_json = serde_json::json!({
        "type": "object",
        "properties": {"access_token": {"type": "string"}, "status": {"type": "string"}},
        "required": ["access_token", "status"]
    })
    .to_string();
    let child = tree.spawn_agent("root", &request, None).unwrap();
    let sibling = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    let child_locator = create_quiescent(&paths, "trace-fixture");
    tree.bind_session_locator(&child.agent_id, child_locator.to_string())
        .unwrap();
    let submitted = r#"{"access_token":"synthetic-result-value","status":"ready"}"#;
    tree.submit_result(
        &child.agent_id,
        &SubmitResultRequest {
            turn_token: tree.attempt(&child.agent_id).unwrap().turn_token,
            output_json: submitted.to_owned(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.set_active_turn("root", true).unwrap();
    tree.set_active_turn(&sibling.agent_id, true).unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let mut product = ProductStore::open(&paths).unwrap();
    product.commit(&mut record).unwrap();
    product.set_archived(&record.id, true).unwrap();
    drop(product);

    let address = format!("{}.{}", record.id, child.agent_id);
    let busy = format!("{}.{}", record.id, sibling.agent_id);
    let path = paths.home().join("trace.json");
    let error = write_trace(&paths, &busy, Some(path.clone()), &mut Vec::new()).unwrap_err();
    assert!(error.message().contains("not settled (Running)"));
    assert!(!path.exists());
    write_trace(&paths, &address, Some(path.clone()), &mut Vec::new()).unwrap();
    let bytes = fs::read(&path).unwrap();
    let trace: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(trace["source"]["head"], child_locator.to_string());
    assert_eq!(trace["source"]["child_id"], address);
    assert_redacted_result(&trace["attempt"]["result"], submitted);
    assert!(trace["mail"].as_array().unwrap().iter().any(|mail| {
        mail["purpose"] == "result" && mail["result_attempt_id"] == trace["source"]["attempt_id"]
    }));
    assert!(write_trace(&paths, &address, Some(path.clone()), &mut Vec::new()).is_err());
    assert_eq!(fs::read(path).unwrap(), bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(paths.home().join("trace.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let mut listed = Vec::new();
    let mut warnings = Vec::new();
    list_traces(&paths, None, &mut listed, &mut warnings).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&listed).unwrap(),
        serde_json::json!([trace["source"]])
    );
    let end = trace["source"]["settled_at_unix_ms"].as_u64().unwrap();
    let since = OffsetDateTime::from_unix_timestamp_nanos(i128::from(end) * 1_000_000).unwrap();
    for (cutoff, expected) in [
        (since, serde_json::json!([trace["source"]])),
        (
            since + time::Duration::nanoseconds(1),
            serde_json::json!([]),
        ),
    ] {
        listed.clear();
        list_traces(&paths, Some(cutoff), &mut listed, &mut warnings).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&listed).unwrap(), expected);
    }
    assert_eq!(warnings.as_slice(), b"");
}

fn assert_redacted_result(result: &Value, submitted: &str) {
    let redacted = r#"{"access_token":"[redacted]","status":"ready"}"#;
    assert_eq!(result["output_json"], redacted);
    assert_eq!(
        result["source_sha256"],
        plakous::model::sha256_hex(&[redacted.as_bytes()])
    );
    assert_ne!(
        result["source_sha256"],
        plakous::model::sha256_hex(&[submitted.as_bytes()])
    );
}

fn completed_child(tree: &mut ProductAgentTree, locator: SessionLocator) -> String {
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    tree.bind_session_locator(&child.agent_id, locator.to_string())
        .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    child.agent_id
}

fn tree_fixture(paths: &PlakPaths, workspace: &std::path::Path) -> (String, ProductAgentTree) {
    paths.ensure_layout().unwrap();
    let mut record = SessionRecord::new(workspace.to_owned());
    ProductStore::open(paths)
        .unwrap()
        .commit(&mut record)
        .unwrap();
    let tree =
        plakous_hosted::topology_runtime::test_support::bootstrap(&record.id, workspace.to_owned());
    (record.id, tree)
}

#[test]
fn trace_receipt_cli_refuses_mismatches_and_never_replaces_an_attestation() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    let (root, mut tree) = tree_fixture(&paths, repo.path());
    let paths = paths.for_workspace(repo.path());
    let locator = create_quiescent(&paths, "trace-receipt");
    let child = completed_child(&mut tree, locator);
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let source = capture(&paths, &root, &child).unwrap().source;
    let run = |attempt: &str, head: &str, revision: u64, sha256: &str| {
        let crate::command::Command::SessionTraceReceipt(record) = crate::cli::parse(
            [
                "session",
                "trace-receipt",
                "record",
                &source.child_id,
                "--attempt",
                attempt,
                "--head",
                head,
                "--revision",
                &revision.to_string(),
                "--sha256",
                sha256,
                "--archive",
                "fixture-artifact",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap() else {
            panic!("expected receipt record command");
        };
        record_receipt(&paths, record)
    };
    let digest = "a".repeat(64);
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let attempt = &tree.attempt(&child).unwrap().attempt_id;
    let lookup = || {
        store
            .child_archive_receipt(&root, &child, attempt, &source.head, source.revision)
            .unwrap()
    };
    for (attempt, head, revision) in [
        ("unrelated-attempt", source.head.as_str(), source.revision),
        (
            source.attempt_id.as_str(),
            "unrelated-head",
            source.revision,
        ),
        (
            source.attempt_id.as_str(),
            source.head.as_str(),
            source.revision + 1,
        ),
    ] {
        let error = run(attempt, head, revision, &digest).unwrap_err();
        assert!(error.message().contains("does not match"), "{error}");
        assert!(lookup().is_none());
    }
    let record = |digest| run(&source.attempt_id, &source.head, source.revision, digest);
    assert!(record("not-a-digest").is_err());
    record(&digest).unwrap();
    record(&digest).unwrap();
    assert!(
        record(&"b".repeat(64))
            .unwrap_err()
            .message()
            .contains("conflicting")
    );
    assert_eq!(lookup().unwrap().sha256, digest);
}

#[test]
fn post_read_recheck_refuses_a_child_that_changed_head() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    let (root, mut tree) = tree_fixture(&paths, repo.path());
    let initial = create_quiescent(&paths, "trace-initial");
    let child = completed_child(&mut tree, initial);
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();

    let TreeInspection::Tree(snapshot) = selected_tree(&paths, &root).unwrap() else {
        panic!("persisted child tree");
    };
    let selected = settled_child(snapshot.rows(), &child).unwrap();
    let (revision, _, _) =
        AgentSessions::inspect_settled_history_sqlite(paths.runtime_database(), initial).unwrap();
    let source = TraceSource::from_child(&root, snapshot.rows(), &selected, revision);
    tree.set_active_turn("root", true).unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    recheck_capture(&paths, &source, &selected).unwrap();

    let next = create_quiescent(&paths, "trace-next");
    tree.advance_session_locator(&child, next.to_string())
        .unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let error = recheck_capture(&paths, &source, &selected).unwrap_err();
    assert_eq!(error.message(), "child changed during trace read; retry");
    assert_eq!(
        capture(&paths, &root, &child).unwrap().source.head,
        next.to_string()
    );
}

#[test]
fn capture_refuses_unfacted_child_input_at_a_quiescent_head() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    let (root, mut tree) = tree_fixture(&paths, repo.path());
    let head = create_quiescent(&paths, "trace-pending-input");
    let child = completed_child(&mut tree, head);
    tree.record_command(AgentCommand::intent(
        "child-command-fixture",
        &child,
        Some(tree.attempt(&child).unwrap().attempt_id.clone()),
        AgentCommandKind::MailStart,
        "child input",
        Vec::new(),
    ))
    .unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let paths = paths.for_workspace(repo.path());
    let out = paths.home().join("unpublished.json");
    let error = write_trace(
        &paths,
        &format!("{root}.{child}"),
        Some(out.clone()),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert_eq!(error.message(), "child has pending input");
    assert!(!out.exists());
}

fn ambiguous_root(paths: &PlakPaths, workspace: &std::path::Path) -> String {
    let mut store = ProductStore::open(paths).unwrap();
    let mut root = SessionRecord::new(workspace.to_owned());
    store.commit(&mut root).unwrap();
    for index in 1..=2 {
        let mut successor = SessionRecord::new(workspace.to_owned());
        successor.node = root.node.clone();
        successor.source_product_session_id = Some(root.id.clone());
        successor.source_workflow_operation_id = Some(format!("plak:rebind:account:{index:02}"));
        store.commit(&mut successor).unwrap();
    }
    root.id
}

#[test]
fn inventory_keeps_good_entries_and_reports_ambiguous_roots_and_stale_heads() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    let (root, mut tree) = tree_fixture(&paths, repo.path());
    let healthy = completed_child(&mut tree, create_quiescent(&paths, "trace-healthy"));
    let current = create_quiescent(&paths, "trace-stale");
    let stale: SessionLocator = format!(
        "{}/{}/00000000-0000-0000-0000-0000000000ff",
        current.session_id(),
        current.branch_id()
    )
    .parse()
    .unwrap();
    let stale_child = completed_child(&mut tree, stale);
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let ambiguous = ambiguous_root(&paths, repo.path());

    let bad_address = format!("{root}.{stale_child}");
    let paths = paths.for_workspace(repo.path());
    let out = paths.home().join("stale.json");
    let error = write_trace(&paths, &bad_address, Some(out.clone()), &mut Vec::new()).unwrap_err();
    assert!(error.message().contains("child trace read failed:"));
    assert!(!out.exists());

    let (mut listed, mut warnings) = (Vec::new(), Vec::new());
    list_traces(&paths, None, &mut listed, &mut warnings).unwrap();
    let listed: Vec<Value> = serde_json::from_slice(&listed).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["child_id"], format!("{root}.{healthy}"));
    let warnings = String::from_utf8(warnings).unwrap();
    assert!(warnings.contains(&format!("{ambiguous:?}: reason=ambiguous_root")));
    assert!(warnings.contains(&format!(
        "{bad_address:?}: reason=runtime_read kind=StaleLocator"
    )));
}

fn populate_inventory(paths: &PlakPaths, workspace: &std::path::Path) {
    let (_, mut tree) = tree_fixture(paths, workspace);
    completed_child(&mut tree, create_quiescent(paths, "trace-inventory"));
    crate::store::tree::save_tree(paths, &mut tree).unwrap();
}

fn assert_inventory_fails_without_json(paths: &PlakPaths, store: &str) {
    let mut stdout = Vec::new();
    let error = list_traces(paths, None, &mut stdout, &mut Vec::new()).unwrap_err();
    assert_ne!(error.exit_code(), 0);
    assert!(error.message().contains(store), "{}", error.message());
    assert_eq!(stdout.as_slice(), b"");
}

#[test]
fn inventory_refuses_a_removed_runtime_database() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    populate_inventory(&paths, repo.path());
    let database = paths.runtime_database();
    fs::rename(&database, database.with_extension("removed")).unwrap();

    assert_inventory_fails_without_json(&paths, "trace inventory runtime store");
}

#[test]
fn inventory_distinguishes_an_empty_home_from_a_removed_tree_store() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let mut stdout = Vec::new();
    list_traces(&paths, None, &mut stdout, &mut Vec::new()).unwrap();
    assert_eq!(stdout.as_slice(), b"[]\n");
    assert!(!paths.runtime_database().exists());
    assert!(!paths.subagents_database().exists());

    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    populate_inventory(&paths, repo.path());
    let database = paths.subagents_database();
    fs::rename(&database, database.with_extension("removed")).unwrap();

    assert_inventory_fails_without_json(&paths, "trace inventory tree store is missing");
}

#[test]
fn trace_v1_golden() {
    let document = Trace {
        format: FORMAT,
        version: VERSION,
        state_key: "example-state".to_owned(),
        producer_binary_id: None,
        captured_at_unix_ms: 30,
        source: TraceSource {
            child_id: "00000000-0000-0000-0000-000000000001.c1".to_owned(),
            child_agent_id: "c1".to_owned(),
            tree_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            root_session_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            root_agent_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            parent_agent_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            attempt_id: "c1-a1".to_owned(),
            head: "00000000-0000-0000-0000-000000000010/00000000-0000-0000-0000-000000000011/00000000-0000-0000-0000-000000000012".to_owned(),
            runtime_session_id: "00000000-0000-0000-0000-000000000010".to_owned(),
            branch_id: "00000000-0000-0000-0000-000000000011".to_owned(),
            checkpoint_id: "00000000-0000-0000-0000-000000000012".to_owned(),
            revision: 2,
            settled_at_unix_ms: 20,
        },
        model: "fixture-model".to_owned(),
        route: "fixture-route".to_owned(),
        effort: Some("high".to_owned()),
        attempt: Attempt {
            status: "completed",
            retry_of: None,
            started_at_unix_ms: 10,
            failure_kind: None,
            result: None,
        },
        mail: Vec::new(),
        history: TraceHistory::encode(&history()).unwrap(),
        unavailable: EXCLUSIONS,
    };
    let expected = serde_json::from_str::<Value>(include_str!("v1.json")).unwrap();
    assert_eq!(serde_json::to_value(document).unwrap(), expected);
}
