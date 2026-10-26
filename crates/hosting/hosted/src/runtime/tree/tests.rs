#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;

use super::*;
use crate::runtime::*;

use crate::runtime::test_kit::MemoryRows;
use crate::runtime::test_support::*;
use crate::sessions::SessionRecord;

#[test]
fn closing_an_agent_without_a_hub_retires_its_cached_notice_scope() {
    use plakous_host_protocol::views::transcript::TranscriptScope;
    let directory = tempfile::tempdir().unwrap();
    let rows = MemoryRows::new();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        directory.path().join("home"),
        directory.path().join("state"),
    );
    let executor = ApplicationExecutor::new().unwrap();
    let session = SessionRecord::new(directory.path().to_path_buf());
    let (wire, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(wire);
    assert!(events.replace_transcript(logging::project_session_snapshot(&session)));
    child_live::send_agent_transcript(&events, "c1", Err("no agent tree".into()));
    let _ = receiver.try_iter().count();
    dispatch_topology_navigator_sync(
        &executor,
        &rows,
        &roots,
        &session,
        &events,
        &UserAction::CloseAgentTranscript,
        &mut TreeSnapshotPush::default(),
    );
    assert!(!events.resync_transcript(&TranscriptScope::Agent("c1".into())));
    assert!(events.resync_transcript(&TranscriptScope::Root));
}

/// A finished child's result keeps 16 KiB and a cut says so and from how
/// much, instead of ending mid-token behind `…`; multi-line text is cut
/// at a line end.
#[test]
fn a_cut_milestone_text_says_it_was_cut() {
    assert_eq!(
        result_text(r#" {"verdict":"ok"} "#).as_deref(),
        Some(r#"{"verdict":"ok"}"#)
    );
    assert_eq!(result_text(" "), None);

    let long = format!(r#"{{"notes":[{}]}}"#, vec![r#""ab""#; 8_000].join(","));
    let text = result_text(&long).unwrap();
    assert!(text.len() <= RESULT_TEXT_BYTES, "{}", text.len());
    let (head, last) = text.split_once('\n').unwrap();
    assert!(head.starts_with(r#"{"notes":["ab","#), "{head}");
    assert_eq!(last, "… cut at 16 KiB of 40 KiB");

    let lines = "a reason line\n".repeat(400);
    let text = milestone_text(&lines).unwrap();
    assert!(text.len() <= MILESTONE_TEXT_BYTES);
    let (head, last) = text.rsplit_once('\n').unwrap();
    assert!(head.ends_with("a reason line"), "{head:?}");
    assert_eq!(last, "… cut at 2 KiB of 6 KiB");

    let reason = "é".repeat(MILESTONE_TEXT_BYTES);
    let text = milestone_text(&reason).unwrap();
    assert!(text.len() <= MILESTONE_TEXT_BYTES);
    assert!(text.ends_with("é\n… cut at 2 KiB of 4 KiB"), "{text}");
}

/// Host `ToolLine` carries the owning marker call for a nested tool.
///
/// The grouping key the renderer needs is projected from the
/// event's caller lineage, never parsed out of a display string. A direct
/// call and the outer marker itself carry no parent.
#[test]
fn tool_line_parent_call_id_projects_event_caller_lineage() {
    use plakous::agent::{ToolCallerClass, ToolCallerLineageView};
    use plakous::model::{CallId, ToolCallerLineage};

    let parent = CallId::try_from(uuid::Uuid::from_u128(0x0104)).unwrap();
    let child = CallId::try_from(uuid::Uuid::from_u128(0x0105)).unwrap();

    let direct = ToolCallerLineageView::direct();
    assert_eq!(direct.class(), ToolCallerClass::Direct);
    assert_eq!(lineage_parent_call_id(&direct), None);

    let marker = ToolCallerLineageView::local_composition_marker(Some("cell-1"));
    assert_eq!(marker.class(), ToolCallerClass::LocalProgrammatic);
    assert!(marker.is_composition_marker());
    assert_eq!(lineage_parent_call_id(&marker), None);

    let canonical = ToolCallerLineage::local_programmatic(7, 1, "cell-1", 2, child, 1).unwrap();
    let nested = ToolCallerLineageView::from_lineage(&canonical, Some(parent));
    assert_eq!(nested.class(), ToolCallerClass::LocalProgrammatic);
    assert_eq!(nested.cell_id(), Some("cell-1"));
    assert_eq!(nested.nested_ordinal(), Some(2));
    assert_eq!(nested.depth(), Some(1));
    assert!(!nested.is_composition_marker());
    assert_eq!(
        lineage_parent_call_id(&nested),
        Some(parent.as_uuid().to_string())
    );

    // The projection is exactly what the emitted line carries.
    let line = HostEvent::ToolLine {
        call_id: Some(child.as_uuid().to_string()),
        parent_call_id: lineage_parent_call_id(&nested),
        cell_id: nested.cell_id().map(str::to_owned),
        kind: plakous_host_protocol::views::presentation::ActivityKind::Tool,
        name: "read_file".into(),
        state: "running".into(),
        subject: None,
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    };
    let HostEvent::ToolLine {
        parent_call_id: emitted,
        ..
    } = line
    else {
        unreachable!("constructed a ToolLine")
    };
    assert_eq!(emitted, Some(parent.as_uuid().to_string()));
}

#[tokio::test]
async fn tree_snapshot_pushes_on_any_node_revision_and_drops_non_advancing() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

    let root = tempfile::tempdir().unwrap();
    let paths = MemoryRows::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let tree_id = durable_tree_id(&paths, &session);
    let tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
    let (sender, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(sender);
    let mut last_revision = TreeSnapshotPush::default();
    let home = SessionHome::new(std::env::temp_dir().join("plak-tree-test-sessions"));

    // The first observation only records the revision: attach already drew.
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut session,
        &mut last_revision,
        &events,
        true,
    );
    assert!(received.try_recv().is_err());

    // A child spawn is a node revision with no root tool receipt behind it.
    {
        let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
        let mut tree = runtime.tree_handle_for_test().lock().unwrap();
        tree.spawn_agent("root", &SpawnAgentRequest::test_review(), Some("op-spawn"))
            .unwrap();
    }
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut session,
        &mut last_revision,
        &events,
        false,
    );
    // The spawn is a persisted milestone row first, then the canvas push.
    let HostEvent::Row(TranscriptRow::Milestone(spawned)) = received.try_recv().unwrap() else {
        panic!("child spawn must publish its milestone row");
    };
    assert_eq!(spawned.event, MilestoneEvent::Spawned);
    assert!(
        session
            .transcript
            .contains(&TranscriptRow::Milestone(spawned))
    );
    let HostEvent::TopologyNavigatorSnapshot(snapshot) = received.try_recv().unwrap() else {
        panic!("child spawn must reach the canvas");
    };
    assert_eq!(snapshot.nodes.len(), 2);
    assert!(!snapshot.solicited);

    // The same revision is never sent twice.
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut session,
        &mut last_revision,
        &events,
        false,
    );
    assert!(received.try_recv().is_err(), "non-advancing revision drops");

    // A status transition on the child is another revision, and it pushes.
    let child = snapshot.nodes[1].agent_id.clone();
    {
        let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
        let mut tree = runtime.tree_handle_for_test().lock().unwrap();
        tree.set_active_turn(&child, true).unwrap();
    }
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut session,
        &mut last_revision,
        &events,
        false,
    );
    let HostEvent::TopologyNavigatorSnapshot(snapshot) = received.try_recv().unwrap() else {
        panic!("a child status transition must reach the canvas");
    };
    assert!(snapshot.nodes[1].active_turn);

    // A question the child addresses to the root is an `asks` milestone,
    // persisted like the others and said once.
    {
        let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
        let mut tree = runtime.tree_handle_for_test().lock().unwrap();
        tree.send_agent_message(
            &child,
            &crate::agent_tree_protocol::SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "which branch?\nmain or a2".into(),
                purpose: Some(crate::agent_tree_protocol::MessagePurpose::Question),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    }
    for _ in 0..2 {
        emit_tree_snapshot_if_revision_advanced(
            &paths,
            paths.records(),
            &home,
            &mut session,
            &mut last_revision,
            &events,
            false,
        );
    }
    let asks: Vec<_> = received
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Milestone(milestone)) => Some(milestone),
            _ => None,
        })
        .collect();
    let [asked] = asks.as_slice() else {
        panic!("one asks milestone expected: {asks:?}");
    };
    assert_eq!(asked.event, MilestoneEvent::Asks);
    assert_eq!(asked.agent_id, child);
    assert_eq!(asked.text.as_deref(), Some("which branch?\nmain or a2"));
    assert!(
        session
            .transcript
            .contains(&TranscriptRow::Milestone(asked.clone()))
    );

    // A finished child carries its submitted result, a failed one its
    // failure message, live and persisted alike.
    let failing = {
        let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
        let mut tree = runtime.tree_handle_for_test().lock().unwrap();
        tree.submit_result(
            &child,
            &crate::agent_tree_protocol::SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child).unwrap();
        tree.spawn_agent(
            "root",
            &SpawnAgentRequest::test_review(),
            Some("op-spawn-2"),
        )
        .unwrap()
        .agent_id
    };
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut session,
        &mut last_revision,
        &events,
        false,
    );
    {
        let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
        let mut tree = runtime.tree_handle_for_test().lock().unwrap();
        tree.record_turn_failure(
            &failing,
            crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                kind: "provider_error",
            },
            "the provider refused the request\nretry later".into(),
        )
        .unwrap();
    }
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut session,
        &mut last_revision,
        &events,
        false,
    );
    let terminal: Vec<_> = received
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Milestone(milestone))
                if matches!(
                    milestone.event,
                    MilestoneEvent::Finished | MilestoneEvent::Failed
                ) =>
            {
                Some(milestone)
            }
            _ => None,
        })
        .collect();
    let [finished, failed] = terminal.as_slice() else {
        panic!("a finished and a failed milestone expected: {terminal:?}");
    };
    assert_eq!(finished.agent_id, child);
    assert_eq!(finished.text.as_deref(), Some(r#"{"ok":true}"#));
    assert_eq!(failed.agent_id, failing);
    assert_eq!(
        failed.text.as_deref(),
        Some("the provider refused the request\nretry later")
    );
    for row in [finished, failed] {
        assert!(
            session
                .transcript
                .contains(&TranscriptRow::Milestone(row.clone()))
        );
    }
    crate::topology_runtime::force_release(&tree_id).await;
}

/// The durable tree id is resolved once per bound session, never per event.
///
/// `durable_topology_tree_id` walks the session's rebind lineage and opens
/// the product store on a hub miss, so calling it after every streamed
/// event pays a sqlite open per `TextDelta`.
#[tokio::test]
async fn the_tree_push_resolves_its_tree_id_once_per_bound_session() {
    let root = tempfile::tempdir().unwrap();
    let paths = MemoryRows::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let tree_id = durable_tree_id(&paths, &session);
    crate::topology_runtime::insert_root(
        tree_id.clone(),
        crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            &tree_id,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap(),
        None,
    )
    .unwrap();
    let mut push = TreeSnapshotPush::default();
    assert!(push.runtime(&paths, &session).is_some());
    assert_eq!(push.tree_id.as_deref(), Some(tree_id.as_str()));

    // A later lineage change on the same session cannot re-resolve it:
    // one bound session owns one tree id.
    session.source_product_session_id = Some(uuid::Uuid::new_v4().to_string());
    assert!(push.runtime(&paths, &session).is_some());
    assert_eq!(push.tree_id.as_deref(), Some(tree_id.as_str()));

    // Once the tree leaves the hub the emitter
    // still answers from two hub-map probes and never re-walks the lineage,
    // so a streamed event costs no store open.
    crate::topology_runtime::force_release(&tree_id).await;
    assert!(push.runtime(&paths, &session).is_none());
    assert_eq!(push.tree_id.as_deref(), Some(tree_id.as_str()));
}

/// A child's abandoned turn reaches the caller's face, once.
///
/// A child session is reopened inside the caller's tool work, where no face
/// is listening, so the reopen leaves the fact on the node. This is the
/// caller reading it. The line names the node, because a caller may be
/// running several, and it says the request may have been billed, which is
/// the operator's only evidence that a provider call may have been charged
/// for output nobody kept. Saying it on every snapshot would make a
/// one-off read as a standing condition, so it is said once.
#[tokio::test]
async fn a_childs_abandoned_turn_reaches_the_callers_face_once() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

    let root = tempfile::tempdir().unwrap();
    let paths = MemoryRows::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let tree_id = durable_tree_id(&paths, &session);
    let tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
    let (sender, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(sender);
    let mut push = TreeSnapshotPush::default();
    let home = SessionHome::new(root.path().join("sessions"));
    {
        let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
        let mut tree = runtime.tree_handle_for_test().lock().unwrap();
        let spawned = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), Some("op-spawn"))
            .unwrap();
        tree.record_abandoned_turn(
            &spawned.agent_id,
            crate::agent_tree::AbandonedChildTurn {
                operations: vec![7],
                claim: plakous::agent::AbandonedTurnClaim::ModelRequest,
            },
        )
        .unwrap();
    }
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut session,
        &mut push,
        &events,
        false,
    );
    let mut notes = Vec::new();
    while let Ok(event) = received.try_recv() {
        if let HostEvent::Row(TranscriptRow::Notice(
            plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
        )) = event
        {
            notes.push(note);
        }
    }
    let abandoned: Vec<&String> = notes
        .iter()
        .filter(|note| note.contains("recovered an interrupted turn"))
        .collect();
    assert_eq!(abandoned.len(), 1, "{notes:?}");
    let note = abandoned[0];
    assert!(note.contains("c1"), "the line names the node: {note}");
    assert!(note.contains("processed and billed"), "{note}");
    assert!(
        note.contains("no response was kept in the conversation"),
        "{note}"
    );

    // A later snapshot of the same tree does not say it again.
    {
        let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
        let mut tree = runtime.tree_handle_for_test().lock().unwrap();
        tree.spawn_agent(
            "root",
            &SpawnAgentRequest::test_review(),
            Some("op-spawn-2"),
        )
        .unwrap();
    }
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut session,
        &mut push,
        &events,
        false,
    );
    while let Ok(event) = received.try_recv() {
        assert!(
            !matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("recovered an interrupted turn")),
            "a one-off must not repeat"
        );
    }
    crate::topology_runtime::force_release(&tree_id).await;
}

/// `/clear`, `/resume`, fork and rebind all replace `config.session`
/// inside one attach. The push cache is keyed on the session it resolved
/// for, so the successor session re-resolves and §5.10 live updates keep
/// arriving instead of dying silently for the rest of the attach.
#[tokio::test]
async fn a_session_switch_re_resolves_the_tree_and_pushes_keep_arriving() {
    let root = tempfile::tempdir().unwrap();
    let paths = MemoryRows::new();
    let mut first = SessionRecord::new(root.path().to_path_buf());
    let first_tree = durable_tree_id(&paths, &first);
    crate::topology_runtime::insert_root(
        first_tree.clone(),
        crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            &first_tree,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap(),
        None,
    )
    .unwrap();
    let (events, rx) = host_event_sink();
    let mut push = TreeSnapshotPush::default();
    let home = SessionHome::new(std::env::temp_dir().join("plak-tree-test-sessions"));
    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut first,
        &mut push,
        &events,
        false,
    );
    assert!(matches!(
        rx.try_iter().collect::<Vec<_>>().as_slice(),
        [HostEvent::TopologyNavigatorSnapshot(_)]
    ));

    // The operator runs `/clear`: a new session, a new tree, same attach.
    let mut second = SessionRecord::new(root.path().to_path_buf());
    let second_tree = durable_tree_id(&paths, &second);
    assert_ne!(first_tree, second_tree);
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        &second_tree,
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    tree.spawn_agent(
        "root",
        &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
        None,
    )
    .unwrap();
    crate::topology_runtime::insert_root(second_tree.clone(), tree, None).unwrap();

    emit_tree_snapshot_if_revision_advanced(
        &paths,
        paths.records(),
        &home,
        &mut second,
        &mut push,
        &events,
        false,
    );
    let observed: Vec<HostEvent> = rx.try_iter().collect();
    let [HostEvent::TopologyNavigatorSnapshot(snapshot)] = observed.as_slice() else {
        panic!("a session switch must not silence the canvas: {observed:?}");
    };
    assert_eq!(snapshot.tree_id, second_tree);
    assert_eq!(snapshot.nodes.len(), 2);
    crate::topology_runtime::force_release(&first_tree).await;
    crate::topology_runtime::force_release(&second_tree).await;
}
