//! Indexed WAL cuts of a frozen, production-driver tree commit sequence.
//!
//! The uncut driver records exact rows and feed inputs before any cut runs.
//! Freezing the sequence keeps host observation clocks identical across cuts.
#![cfg(all(feature = "commit-trace", feature = "test-support", unix))]
#![allow(clippy::unwrap_used, reason = "deterministic offline law fixtures")]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use plakous::agent::{AgentErrorKind, CommandId, TurnUsage};
use plakous::model::{CallId, EntryId, FinishReason, HistorySegmentKind, TurnId, TurnOrigin};
use plakous::sessions::feed::{FeedCommandKind, FeedFact, FeedHeader, FeedRecord, FeedTurnOutcome};
use plakous_recursive_subagents::protocol::{
    AttemptScopedRequest, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest, SpawnWorkspace,
    SubmitResultRequest, Tier, WorkspaceIdRequest, WriterMode,
};
use plakous_recursive_subagents::tools::TreeDriver;
use plakous_recursive_subagents::tree::{
    AgentCommand, AgentCommandKind, ChildFinalizationDecision, FailureRead, FakeOp, FakeVcs,
    PersistedTree, ProductAgentTree, Revision, TreeFeedReads, TreeStore, TreeStoreError,
};
use plakous_recursive_subagents_store::{SqliteTreeStore, TreeInspection, rows};
use serde_json::{Value, json};
use sha2::Digest;

const TREE: &str = "commit-cut-tree";
const CHILD_ENV: &str = "PLAK_TREE_MATRIX_FIXTURE";

#[derive(Clone, Default)]
struct Feed {
    records: Vec<FeedRecord>,
    reads: TreeFeedReads,
    wait_evidence: Option<bool>,
}

impl Feed {
    fn wait_evidence(&mut self, committed: bool) {
        let header = self.records.last().unwrap().header();
        let locator = plakous::sessions::SessionLocator::new(
            header.session_id,
            header.branch_id,
            uuid(0x3000 + header.seq.get()).parse().unwrap(),
        );
        self.reads = TreeFeedReads::default();
        self.reads
            .insert_waits(locator, vec![(uuid(0x2000), committed)]);
        self.wait_evidence = Some(committed);
    }

    fn push(&mut self, child: usize, facts: Vec<FeedFact>) {
        let seq = self.records.len() as u64 + 1;
        let change = if seq == 1 {
            json!({"created": {"genesis": uuid(0x3000 + seq), "revision": seq}})
        } else {
            json!({"advanced": {"parent": uuid(0x3000 + seq - 1),
                "checkpoint": uuid(0x3000 + seq), "revision": seq, "head": "quiescent"}})
        };
        let header: FeedHeader = serde_json::from_value(json!({
            "session_id": session(child), "seq": seq, "branch_id": uuid(0xb),
            "committed_at_ms": 1_000 * seq, "change": change,
        }))
        .unwrap();
        self.records.push(FeedRecord::from_store(header, facts));
    }
}

fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

fn session(child: usize) -> String {
    uuid(0x100 + child as u64)
}

fn turn(child: usize, round: u64) -> TurnId {
    uuid(0x1000 + child as u64 * 16 + round).parse().unwrap()
}

#[derive(Clone)]
struct Frame {
    tree: ProductAgentTree,
    feeds: Vec<Feed>,
}

struct RecordingStore {
    inner: SqliteTreeStore,
    feeds: Arc<Mutex<Vec<Feed>>>,
    frames: Mutex<Vec<Frame>>,
}

impl TreeStore for RecordingStore {
    fn exists(&self, id: &str) -> Result<bool, TreeStoreError> {
        self.inner.exists(id)
    }

    fn ids(&self) -> Result<Vec<String>, TreeStoreError> {
        self.inner.ids()
    }

    fn load(&self, id: &str) -> Result<Option<PersistedTree>, TreeStoreError> {
        self.inner.load(id)
    }

    fn save(&self, tree: &ProductAgentTree) -> Result<Revision, TreeStoreError> {
        let revision = self.inner.save(tree)?;
        self.frames.lock().unwrap().push(Frame {
            tree: tree.to_persisted().rows().clone(),
            feeds: self.feeds.lock().unwrap().clone(),
        });
        Ok(revision)
    }
}

fn publish<R>(driver: &TreeDriver, work: impl FnOnce(&mut ProductAgentTree) -> R) -> R {
    let (result, effects) = driver
        .mutate(|tree| {
            let result = work(tree);
            driver.save(tree).unwrap();
            result
        })
        .unwrap();
    driver.settle(effects).unwrap();
    result
}

fn project(tree: &mut ProductAgentTree, child: usize, feed: &Feed) {
    let agent = if child == 0 {
        "root".to_owned()
    } else {
        format!("c{child}")
    };
    tree.project_feed(&agent, &session(child), &feed.records, &feed.reads)
        .unwrap();
}

fn admit(driver: &TreeDriver, feeds: &Mutex<Vec<Feed>>, child: usize, round: u64) {
    let agent = format!("c{child}");
    let command = format!("command:{child}:{round}");
    let mut all = feeds.lock().unwrap();
    all[child].push(
        child,
        vec![FeedFact::CommandAccepted {
            command_id: CommandId::new(&command).unwrap(),
            kind: FeedCommandKind::Root {
                origin: TurnOrigin::UserPrompt,
            },
            turn_id: Some(turn(child, round)),
            sequence: round,
            operation_id: None,
        }],
    );
    let feed = all[child].clone();
    drop(all);
    publish(driver, |tree| {
        let attempt = tree.attempt(&agent).unwrap().attempt_id.clone();
        tree.record_command(AgentCommand::intent(
            command,
            &agent,
            Some(attempt),
            AgentCommandKind::Start,
            "assignment",
            Vec::new(),
        ))
        .unwrap();
        project(tree, child, &feed);
    });
}

fn end(
    driver: &TreeDriver,
    feeds: &Mutex<Vec<Feed>>,
    child: usize,
    round: u64,
    decision: ChildFinalizationDecision,
    failed: bool,
) {
    let mut all = feeds.lock().unwrap();
    let feed = &mut all[child];
    if failed {
        feed.reads.insert_failure(
            turn(child, round).to_string(),
            FailureRead {
                kind: AgentErrorKind::Internal,
                retryability: None,
                phase: None,
                http_status: None,
                provider_code: None,
                message: "scripted failure".into(),
            },
        );
    }
    feed.push(
        child,
        vec![FeedFact::TurnTerminal {
            turn_id: turn(child, round),
            command_id: CommandId::new(format!("command:{child}:{round}")).unwrap(),
            outcome: if failed {
                FeedTurnOutcome::Failed {
                    kind: AgentErrorKind::Internal,
                    model_kind: None,
                    context: None,
                }
            } else {
                FeedTurnOutcome::Completed {
                    finish: FinishReason::Stop,
                }
            },
            closed_exchange: !failed,
            steering_settled_unapplied: false,
            usage: TurnUsage::new(plakous::model::Usage::new(100, 10, 0, 0, 0), 1, 1, 1).unwrap(),
        }],
    );
    let feed = feed.clone();
    drop(all);
    publish(driver, |tree| {
        let agent = format!("c{child}");
        if failed {
            let attempt = tree.attempt(&agent).unwrap().attempt_id.clone();
            tree.record_failed_turn_notice(
                &agent,
                &attempt,
                &turn(child, round).to_string(),
                "snapshot notice".into(),
            )
            .unwrap();
        }
        let projection = tree
            .project_feed(&agent, &session(child), &feed.records, &feed.reads)
            .unwrap();
        let terminal = (decision == ChildFinalizationDecision::Finalize)
            .then(|| projection.ended_turns.last().unwrap().1.clone());
        tree.record_child_finalization(
            &agent,
            &turn(child, round).to_string(),
            decision,
            true,
            terminal,
        )
        .unwrap();
        project(tree, child, &feed);
    });
}

#[allow(clippy::too_many_lines, reason = "one scripted commit-cut scenario")]
fn scenario(root: &Path) -> (Vec<Frame>, Arc<FakeVcs>) {
    let vcs = Arc::new(FakeVcs::new());
    let feeds = Arc::new(Mutex::new(vec![Feed::default(); 8]));
    let store = Arc::new(RecordingStore {
        inner: SqliteTreeStore::open(&root.join("recording.sqlite3")).unwrap(),
        feeds: feeds.clone(),
        frames: Mutex::new(Vec::new()),
    });
    let tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        TREE,
        "root",
        "owner",
        root.to_owned(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let driver = TreeDriver::new(Arc::new(Mutex::new(tree)), store.clone(), vcs.clone(), None);
    publish(&driver, |_| ());
    feeds.lock().unwrap()[0].push(0, Vec::new());
    let root_feed = feeds.lock().unwrap()[0].clone();
    publish(&driver, |tree| project(tree, 0, &root_feed));
    for child in 1..=7 {
        publish(&driver, |tree| {
            let mut request = if [1, 6, 7].contains(&child) {
                SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "scripted task")
            } else {
                SpawnAgentRequest::test_review()
            };
            request.output_schema_json = json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object", "properties": {}, "required": [], "additionalProperties": false,
            })
            .to_string();
            if [1, 6, 7].contains(&child) {
                request.workspace = SpawnWorkspace::Fork;
            }
            assert_eq!(
                tree.spawn_agent("root", &request, None).unwrap().agent_id,
                format!("c{child}")
            );
        });
        admit(&driver, &feeds, child, 1);
    }
    publish(&driver, |tree| {
        tree.send_agent_message(
            "c1",
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "question before parking".into(),
                purpose: None,
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    });
    let wait_call: CallId = uuid(0x2000).parse().unwrap();
    feeds.lock().unwrap()[0].wait_evidence(false);
    publish(&driver, |tree| {
        let mail = tree.pending_mail("root");
        assert_eq!(mail.len(), 1);
        tree.acknowledge_mail(&mail[0].message_id).unwrap();
        tree.remember_success(
            &wait_call.to_string(),
            "root",
            "wait_agent",
            &json!({}),
            &json!({
                "output": {}, "acknowledged": mail, "newly_delivered": [],
            }),
        )
        .unwrap();
    });
    let entry: EntryId = uuid(0x4000).parse().unwrap();
    feeds.lock().unwrap()[0].push(
        0,
        vec![FeedFact::HistoryAppended {
            turn_id: None,
            segment: HistorySegmentKind::ToolExchange,
            first_ordinal: 0,
            count: 1,
            first_entry: entry,
            last_entry: entry,
            tool_results: vec![wait_call],
        }],
    );
    feeds.lock().unwrap()[0].wait_evidence(true);
    let root_feed = feeds.lock().unwrap()[0].clone();
    publish(&driver, |tree| project(tree, 0, &root_feed));
    end(
        &driver,
        &feeds,
        1,
        1,
        ChildFinalizationDecision::Park,
        false,
    );
    admit(&driver, &feeds, 1, 2);
    end(
        &driver,
        &feeds,
        1,
        2,
        ChildFinalizationDecision::Nudge,
        false,
    );
    admit(&driver, &feeds, 1, 3);
    publish(&driver, |tree| {
        let token = tree.attempt("c1").unwrap().turn_token;
        tree.submit_result(
            "c1",
            &SubmitResultRequest {
                turn_token: token,
                output_json: "{}".into(),
            },
            None,
        )
        .unwrap();
    });
    end(
        &driver,
        &feeds,
        1,
        3,
        ChildFinalizationDecision::Finalize,
        false,
    );
    end(
        &driver,
        &feeds,
        2,
        1,
        ChildFinalizationDecision::Finalize,
        true,
    );
    publish(&driver, |tree| {
        tree.record_host_cancellation("c3").unwrap();
        tree.fold_due("c3").unwrap();
    });
    publish(&driver, |tree| {
        tree.record_host_retirement("c4", "live release uncertain".into())
            .unwrap();
        tree.fold_due("c4").unwrap();
    });
    publish(&driver, |tree| {
        tree.record_host_materialization("c6").unwrap();
        tree.fold_due("c6").unwrap();
    });
    publish(&driver, |tree| {
        tree.close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: "c5".into(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
        tree.record_host_retirement("c5", "deletion release uncertain".into())
            .unwrap();
        tree.fold_due("c5").unwrap();
    });
    end(
        &driver,
        &feeds,
        7,
        1,
        ChildFinalizationDecision::Finalize,
        false,
    );
    publish(&driver, |tree| {
        tree.close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: "c7".into(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
        let workspace_id = tree.node("c7").unwrap().workspace_id.clone();
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest { workspace_id },
            Some("discard:7"),
        )
        .unwrap();
    });
    let frames = store.frames.lock().unwrap().clone();
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    let final_tree = &frames.last().unwrap().tree;
    assert_eq!(
        final_tree
            .operations()
            .values()
            .filter(|op| op.tool_name == "failed_turn_notice")
            .count(),
        1
    );
    assert_eq!(
        final_tree
            .attempt("c2")
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .kind,
        "turn_failed"
    );
    for agent in ["c1", "c2", "c3", "c6", "c7"] {
        assert_eq!(
            final_tree
                .messages()
                .values()
                .filter(|mail| mail.from == agent
                    && mail.purpose
                        == plakous_recursive_subagents::protocol::MessagePurpose::Result)
                .count(),
            1,
            "{agent} must publish exactly one result"
        );
    }
    (frames, vcs)
}

fn snapshot(tree: &ProductAgentTree) -> Value {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("snapshot.sqlite3");
    write_frame(&path, tree);
    let db = rusqlite::Connection::open(path).unwrap();
    let tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'agent_%' ORDER BY name")
        .unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(
        tables,
        [
            "agent_attempts",
            "agent_candidates",
            "agent_commands",
            "agent_feed_cursors",
            "agent_grants",
            "agent_messages",
            "agent_nodes",
            "agent_operations",
            "agent_trees",
            "agent_workspaces"
        ]
    );
    let mut result = serde_json::Map::new();
    for table in tables {
        let mut statement = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
            .unwrap();
        let columns = statement
            .column_names()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let records = statement
            .query_map([], |row| {
                let mut record = serde_json::Map::new();
                for (index, column) in columns.iter().enumerate() {
                    if table == "agent_trees"
                        && matches!(column.as_str(), "revision" | "host_generation")
                    {
                        continue;
                    }
                    let value = match row.get_ref(index)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(value) => json!(value),
                        rusqlite::types::ValueRef::Text(value) => {
                            json!(std::str::from_utf8(value).unwrap())
                        }
                        _ => panic!("unexpected tree column type"),
                    };
                    record.insert(column.clone(), value);
                }
                Ok(Value::Object(record))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        result.insert(table, json!(records));
    }
    Value::Object(result)
}

#[derive(serde::Serialize)]
struct Fault<'a> {
    source_id: String,
    workspace_id: Option<&'a str>,
    attempt_id: Option<&'a plakous_recursive_subagents::protocol::AttemptId>,
    reason: &'a str,
}

fn faulted_external_records(tree: &ProductAgentTree, expected: &mut Value) {
    let reason = "a VCS seal, adopt, or removal was in flight when the process ended";
    let mut faults = Vec::new();
    for record in tree
        .operations()
        .values()
        .filter(|record| record.tool_name == "seal_effect" && record.terminal_response.is_none())
    {
        let request: Value = serde_json::from_str(&record.canonical_request).unwrap();
        let node = tree.node(&record.caller_agent_id).unwrap();
        faults.push((
            node,
            Fault {
                source_id: record.operation_id.clone(),
                workspace_id: None,
                attempt_id: node.attempt_id.as_ref(),
                reason,
            },
            request["reserved_at"]["seq"].as_u64().unwrap(),
        ));
    }
    for workspace in tree
        .workspaces()
        .values()
        .filter(|workspace| workspace.status.is_in_flight())
    {
        for node in tree.nodes().values().filter(|node| {
            node.workspace_id == workspace.workspace_id
                || workspace.status
                    == plakous_recursive_subagents::protocol::WorkspaceStatus::Adopting
                    && node.agent_id
                        == *workspace
                            .adopter_agent_id
                            .as_ref()
                            .unwrap_or(&workspace.owner_agent_id)
        }) {
            faults.push((node, Fault { source_id: format!("workspace:{}", workspace.workspace_id), workspace_id: Some(&workspace.workspace_id),
                attempt_id: node.attempt_id.as_ref(), reason: if node.agent_id == "root" && workspace.status == plakous_recursive_subagents::protocol::WorkspaceStatus::Adopting {
                    "an adopt into this checkout was in flight when the process ended; inspect it before relying on its contents"
                } else { reason } }, 0));
        }
    }
    let mut order = tree.revision;
    for (node, fault, reserved_seq) in faults {
        let canonical = serde_json::to_string(&fault).unwrap();
        let operations = expected["agent_operations"].as_array_mut().unwrap();
        if operations.iter().any(|record| {
            record["tool_name"] == "vcs_fault" && record["canonical_request"] == canonical
        }) {
            continue;
        }
        let session = if node.agent_id == "root" {
            Some(tree.tree_id.clone())
        } else {
            tree.operations()
                .get(&format!("child_session:{}", node.agent_id))
                .map(|record| {
                    record
                        .canonical_request
                        .parse::<plakous::sessions::SessionLocator>()
                        .unwrap()
                        .session_id()
                        .to_string()
                })
        };
        let cursor = session
            .as_ref()
            .map_or(0, |session| tree.feed_cursor(session));
        let anchor = if cursor != 0 {
            cursor
        } else {
            tree.operations()
                .values()
                .filter(|record| record.caller_agent_id == node.agent_id)
                .filter_map(|record| record.fold.map(|fold| fold.seq))
                .max()
                .unwrap_or(0)
                .max(reserved_seq)
        };
        append_fault_record(
            expected,
            &node.agent_id,
            &canonical,
            session,
            plakous_recursive_subagents::tree::FoldAnchor { seq: anchor, order },
        );
        order += 1;
    }
    expected["agent_operations"]
        .as_array_mut()
        .unwrap()
        .sort_by(|left, right| {
            left["operation_id"]
                .as_str()
                .cmp(&right["operation_id"].as_str())
        });
}

fn append_fault_record(
    expected: &mut Value,
    agent_id: &str,
    canonical: &str,
    session: Option<String>,
    anchor: plakous_recursive_subagents::tree::FoldAnchor,
) {
    let order = anchor.order;
    let mut digest = String::with_capacity(64);
    for byte in sha2::Sha256::digest(canonical.as_bytes()) {
        use std::fmt::Write as _;
        write!(digest, "{byte:02x}").unwrap();
    }
    expected["agent_operations"].as_array_mut().unwrap().push(json!({
            "tree_id": TREE, "operation_id": format!("plak:vcs-fault:{}:{order}", agent_id),
            "caller_agent_id": agent_id, "tool_name": "vcs_fault", "canonical_request": canonical,
            "request_digest": digest, "terminal_code": null, "terminal_response": "effects_reserved",
            "fold_anchor": anchor.seq, "fold_order": anchor.order,
        }));
    for record in expected["agent_nodes"].as_array_mut().unwrap() {
        if record["agent_id"] == agent_id {
            record["fold_order"] = json!(anchor.order);
        }
    }
    if let Some(session) = session
        && !expected["agent_feed_cursors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["runtime_session_id"] == session)
    {
        expected["agent_feed_cursors"].as_array_mut().unwrap().push(
            json!({"tree_id": TREE, "runtime_session_id": session, "applied_seq": anchor.seq}),
        );
        expected["agent_feed_cursors"]
            .as_array_mut()
            .unwrap()
            .sort_by(|left, right| {
                left["runtime_session_id"]
                    .as_str()
                    .cmp(&right["runtime_session_id"].as_str())
            });
    }
}

/// This oracle uses only the uncut frame, never the reopened database or projector.
fn oracle(frame: &Frame) -> (Value, BTreeSet<String>) {
    let tree = &frame.tree;
    let mut expected = snapshot(tree);
    let mut faulted = BTreeSet::new();
    for node in tree.nodes().values() {
        if node.agent_id == "root" || node.status == NodeStatus::Closed {
            continue;
        }
        let workspace = tree.workspace(&node.workspace_id).unwrap();
        if node.status == NodeStatus::Sealing || workspace.status.is_in_flight() {
            faulted.insert(node.agent_id.clone());
        }
    }
    faulted_external_records(tree, &mut expected);
    for node in expected["agent_nodes"].as_array_mut().unwrap() {
        if faulted.contains(node["agent_id"].as_str().unwrap()) {
            node["status"] = json!("recovery_required");
            node["active_turn"] = json!(0);
        }
    }
    // Faulting the last sealing child makes the tree quiescent. Reopen then
    // lifts its restoration-only pause; an obligation is not live work.
    let live = tree.nodes().values().any(|node| {
        node.agent_id != "root" && !faulted.contains(&node.agent_id) && !node.status.is_finished()
    });
    if live {
        expected["agent_trees"][0]["state"] = json!("restored_paused");
    }
    if frame.feeds[0].wait_evidence == Some(false) {
        // The cut's bounded canonical read proves this provisional ack did
        // not commit. The independent oracle returns its mail to unread.
        let operations = expected["agent_operations"].as_array_mut().unwrap();
        let mut restored = BTreeSet::new();
        operations.retain(|operation| {
            if operation["tool_name"] != "wait_agent" {
                return true;
            }
            let receipt: Value =
                serde_json::from_str(operation["terminal_response"].as_str().unwrap()).unwrap();
            for mail in receipt["acknowledged"].as_array().unwrap() {
                restored.insert(mail["message_id"].as_str().unwrap().to_owned());
            }
            false
        });
        for mail in expected["agent_messages"].as_array_mut().unwrap() {
            if restored.contains(mail["message_id"].as_str().unwrap()) {
                mail["durable"] = json!("accepted");
            }
        }
    }
    (expected, faulted)
}

fn write_frame(path: &Path, tree: &ProductAgentTree) {
    drop(SqliteTreeStore::open(path).unwrap());
    let mut db = rusqlite::Connection::open(path).unwrap();
    let tx = db.transaction().unwrap();
    rows::insert_agent_tree_rows(&tx, tree).unwrap();
    tx.commit().unwrap();
}

fn inspect(path: &Path) -> PersistedTree {
    let TreeInspection::Tree(tree) = SqliteTreeStore::inspect(path, TREE).unwrap() else {
        panic!("recorded tree is absent");
    };
    *tree
}

fn assert_views(actual: &Value, expected: &Value, cut: u64) {
    for (table, expected) in expected.as_object().unwrap() {
        let actual = actual[table].as_array().unwrap();
        let expected = expected.as_array().unwrap();
        assert_eq!(actual.len(), expected.len(), "cut {cut}: {table} row count");
        for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            for (column, value) in expected.as_object().unwrap() {
                assert_eq!(
                    &actual[column], value,
                    "cut {cut}: {table}[{index}].{column}"
                );
            }
        }
    }
}

#[test]
fn tree_matrix_child() {
    let Some(root) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let root = PathBuf::from(root);
    let store = SqliteTreeStore::open(&root.join("cut.sqlite3")).unwrap();
    let count: usize = std::fs::read_to_string(root.join("count"))
        .unwrap()
        .parse()
        .unwrap();
    let mut previous = 0;
    for index in 0..count {
        let mut tree = inspect(&root.join(format!("frame-{index}.sqlite3")))
            .rows()
            .clone();
        tree.stored_revision = previous;
        let _label = plakous_store::commit_trace::label("tree-matrix-frame");
        tree.acknowledge_revision(store.save(&tree).unwrap());
        previous = tree.revision;
    }
}

fn run_child(root: &Path, cut: Option<u64>) -> std::process::Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "tree_matrix_child", "--nocapture"])
        .env(CHILD_ENV, root)
        .env(
            plakous_store::commit_trace::TRACE_ENV,
            root.join("trace.jsonl"),
        );
    if let Some(cut) = cut {
        command.env(plakous_store::commit_trace::KILL_ENV, format!("tree:{cut}"));
    }
    command.output().unwrap()
}

fn trace(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("trace.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn open(store: &SqliteTreeStore, feeds: &[Feed], vcs: &Arc<FakeVcs>) -> ProductAgentTree {
    let mut tree = ProductAgentTree::reopen(store.load(TREE).unwrap().unwrap());
    tree.set_vcs(vcs.clone());
    for (child, feed) in feeds.iter().enumerate() {
        if !feed.records.is_empty() {
            project(&mut tree, child, feed);
        }
    }
    assert!(
        tree.take_effects().is_empty(),
        "reopen must not reserve a second external effect"
    );
    tree.acknowledge_revision(store.save(&tree).unwrap());
    tree
}

fn assert_no_fault_records(reopened: &ProductAgentTree, ordinal: u64) {
    assert!(
        reopened.operations().values().all(|operation| {
            operation.tool_name != "vcs_fault"
                && !(operation.tool_name == "seal_effect"
                    && operation
                        .terminal_response
                        .as_deref()
                        .is_some_and(|response| {
                            serde_json::from_str::<Value>(response)
                                .is_ok_and(|outcome| outcome["faulted"] == true)
                        }))
        }),
        "cut {ordinal}: ordinary reopen manufactured a VCS fault record"
    );
}

#[test]
fn indexed_tree_store_commit_cut_matrix() {
    use std::os::unix::process::ExitStatusExt;
    let root = tempfile::tempdir().unwrap();
    let (frames, _) = scenario(root.path());
    for (index, frame) in frames.iter().enumerate() {
        write_frame(
            &root.path().join(format!("frame-{index}.sqlite3")),
            &frame.tree,
        );
    }
    std::fs::write(root.path().join("count"), frames.len().to_string()).unwrap();
    let db = root.path().join("cut.sqlite3");
    drop(SqliteTreeStore::open(&db).unwrap());
    let empty = root.path().join("empty.sqlite3");
    std::fs::copy(&db, &empty).unwrap();
    let pilot = run_child(root.path(), None);
    assert!(
        pilot.status.success(),
        "{}",
        String::from_utf8_lossy(&pilot.stderr)
    );
    let pilot_trace = trace(root.path());
    assert_eq!(
        pilot_trace.len(),
        frames.len(),
        "every production-driver save must be indexed"
    );
    assert!(pilot_trace.len() >= 35, "tree commit matrix quietly shrank");
    let mut ordinary = 0;
    let mut ambiguous = 0;
    for (index, commit) in pilot_trace.iter().enumerate() {
        let ordinal = index as u64 + 1;
        assert_eq!(commit["store"], "tree");
        assert_eq!(commit["store_ordinal"], ordinal);
        assert_eq!(commit["label"], "tree-matrix-frame");
        std::fs::remove_file(root.path().join("trace.jsonl")).unwrap();
        std::fs::copy(&empty, &db).unwrap();
        let killed = run_child(root.path(), Some(ordinal));
        assert_eq!(
            killed.status.signal(),
            Some(9),
            "cut {ordinal}: {}",
            String::from_utf8_lossy(&killed.stderr)
        );
        assert_eq!(
            trace(root.path()),
            pilot_trace[..=index],
            "cut {ordinal} changed the commit prefix"
        );
        let frame = &frames[index];
        let (expected, faulted) = oracle(frame);
        let in_flight = frame
            .tree
            .nodes()
            .values()
            .any(|node| node.status == NodeStatus::Sealing)
            || frame
                .tree
                .workspaces()
                .values()
                .any(|workspace| workspace.status.is_in_flight());
        let class = if in_flight {
            ambiguous += 1;
            "b"
        } else {
            ordinary += 1;
            "a"
        };
        let store = SqliteTreeStore::open(&db).unwrap();
        assert_views(
            &snapshot(store.load(TREE).unwrap().unwrap().rows()),
            &snapshot(&frame.tree),
            ordinal,
        );
        let vcs = Arc::new(FakeVcs::new());
        let reopened = open(&store, &frame.feeds, &vcs);
        assert_views(&snapshot(&reopened), &expected, ordinal);
        if !in_flight {
            assert_no_fault_records(&reopened, ordinal);
        }
        for id in &faulted {
            assert!(
                reopened.fault_reason(id).is_some(),
                "cut {ordinal}: missing VCS obligation for {id}"
            );
        }
        let again = open(&store, &frame.feeds, &vcs);
        assert_views(&snapshot(&again), &expected, ordinal);
        assert!(vcs.calls().is_empty(), "cut {ordinal}: reopen re-ran VCS");
        eprintln!("tree:{ordinal} class={class} faulted={faulted:?}");
        drop(store);
    }
    assert!(ordinary >= 30, "ordinary cut matrix quietly shrank");
    assert!(ambiguous >= 3, "VCS obligation cut matrix quietly shrank");
    eprintln!(
        "tree commit cuts={} class_a={ordinary} class_b={ambiguous}",
        pilot_trace.len()
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "one frozen two-save Finalize and cancellation scenario"
)]
fn two_phase_scenario(root: &Path) -> Vec<Frame> {
    let vcs = Arc::new(FakeVcs::new());
    let feeds = Arc::new(Mutex::new(vec![Feed::default(); 3]));
    let store = Arc::new(RecordingStore {
        inner: SqliteTreeStore::open(&root.join("recording.sqlite3")).unwrap(),
        feeds: feeds.clone(),
        frames: Mutex::new(Vec::new()),
    });
    let tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        TREE,
        "root",
        "owner",
        root.to_owned(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let driver = TreeDriver::new(Arc::new(Mutex::new(tree)), store.clone(), vcs, None);
    publish(&driver, |_| ());
    for child in 1..=2 {
        publish(&driver, |tree| {
            let mut request =
                SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "task");
            request.workspace = SpawnWorkspace::Fork;
            tree.spawn_agent("root", &request, None).unwrap();
            tree.record_child_session(
                &format!("c{child}"),
                &format!("{}/{}/{}", session(child), uuid(0xb), uuid(0x3001)),
            )
            .unwrap();
        });
        admit(&driver, &feeds, child, 1);
    }
    {
        let mut all = feeds.lock().unwrap();
        all[1].push(
            1,
            vec![FeedFact::TurnTerminal {
                turn_id: turn(1, 1),
                command_id: CommandId::new("command:1:1").unwrap(),
                outcome: FeedTurnOutcome::Completed {
                    finish: FinishReason::Stop,
                },
                closed_exchange: true,
                steering_settled_unapplied: false,
                usage: TurnUsage::default(),
            }],
        );
    }
    let feed = feeds.lock().unwrap()[1].clone();
    let terminal = publish(&driver, |tree| {
        tree.project_feed("c1", &session(1), &feed.records, &feed.reads)
            .unwrap()
            .ended_turns
            .last()
            .unwrap()
            .1
            .clone()
    });
    publish(&driver, |tree| {
        tree.record_child_finalization(
            "c1",
            &turn(1, 1).to_string(),
            ChildFinalizationDecision::Finalize,
            true,
            Some(terminal),
        )
        .unwrap();
    });
    let (applied, effects) = driver
        .mutate(|tree| driver.fold_and_commit(tree, |tree| tree.fold_due("c1")))
        .unwrap();
    applied.unwrap();
    assert_eq!(effects.seals.len(), 1);
    driver.settle(effects).unwrap();
    publish(&driver, |tree| {
        tree.submit_result(
            "c2",
            &SubmitResultRequest {
                turn_token: tree.attempt("c2").unwrap().turn_token,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
    });
    publish(&driver, |tree| {
        tree.record_host_cancellation("c2").unwrap();
    });
    let (applied, effects) = driver
        .mutate(|tree| driver.fold_and_commit(tree, |tree| tree.fold_due("c2")))
        .unwrap();
    applied.unwrap();
    assert_eq!(effects.seals.len(), 1);
    driver.settle(effects).unwrap();
    store.frames.lock().unwrap().clone()
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "indexed two-save cuts and their no-repeat oracle"
)]
fn indexed_finalize_and_submitted_cancel_intent_consequence_cuts() {
    use plakous_recursive_subagents::protocol::AttemptStatus;
    use std::os::unix::process::ExitStatusExt;
    let root = tempfile::tempdir().unwrap();
    let frames = two_phase_scenario(root.path());
    let pending: BTreeSet<_> = frames
        .iter()
        .flat_map(|frame| {
            frame
                .tree
                .operations()
                .values()
                .filter(|operation| {
                    matches!(
                        operation.tool_name.as_str(),
                        "child_finalization" | "host_decision"
                    ) && operation.terminal_response.is_none()
                })
                .map(|operation| operation.tool_name.clone())
        })
        .collect();
    assert_eq!(
        pending,
        BTreeSet::from(["child_finalization".to_owned(), "host_decision".to_owned()])
    );
    for (index, frame) in frames.iter().enumerate() {
        write_frame(
            &root.path().join(format!("frame-{index}.sqlite3")),
            &frame.tree,
        );
    }
    std::fs::write(root.path().join("count"), frames.len().to_string()).unwrap();
    let db = root.path().join("cut.sqlite3");
    drop(SqliteTreeStore::open(&db).unwrap());
    let empty = root.path().join("empty.sqlite3");
    std::fs::copy(&db, &empty).unwrap();
    assert!(run_child(root.path(), None).status.success());
    let pilot_trace = trace(root.path());
    assert_eq!(pilot_trace.len(), frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let ordinal = index as u64 + 1;
        std::fs::remove_file(root.path().join("trace.jsonl")).unwrap();
        std::fs::copy(&empty, &db).unwrap();
        assert_eq!(
            run_child(root.path(), Some(ordinal)).status.signal(),
            Some(9)
        );
        assert_eq!(trace(root.path()), pilot_trace[..=index]);
        let store = SqliteTreeStore::open(&db).unwrap();
        assert_views(
            &snapshot(store.load(TREE).unwrap().unwrap().rows()),
            &snapshot(&frame.tree),
            ordinal,
        );
        let vcs = Arc::new(FakeVcs::new());
        let mut reopened = ProductAgentTree::reopen(store.load(TREE).unwrap().unwrap());
        reopened.set_vcs(vcs.clone());
        for (child, feed) in frame.feeds.iter().enumerate() {
            if !feed.records.is_empty() {
                project(&mut reopened, child, feed);
            }
        }
        let effects = reopened.take_effects();
        assert!(
            effects.seals.len() <= 1,
            "cut {ordinal}: duplicate seal ticket"
        );
        reopened.acknowledge_revision(store.save(&reopened).unwrap());
        let _ = reopened.complete(effects.run(&*vcs));
        reopened.acknowledge_revision(store.save(&reopened).unwrap());
        let mut again = ProductAgentTree::reopen(store.load(TREE).unwrap().unwrap());
        for (child, feed) in frame.feeds.iter().enumerate() {
            if !feed.records.is_empty() {
                project(&mut again, child, feed);
            }
        }
        assert!(
            again.take_effects().is_empty(),
            "cut {ordinal}: second reopen reserved an effect"
        );
        for (agent, kind, status) in [
            (
                "c1",
                "child_finalization",
                AttemptStatus::CompletedWithoutSubmission,
            ),
            ("c2", "host_decision", AttemptStatus::Completed),
        ] {
            if !frame
                .tree
                .operations()
                .values()
                .any(|operation| operation.caller_agent_id == agent && operation.tool_name == kind)
            {
                continue;
            }
            assert_eq!(
                reopened.attempt(agent).unwrap().status,
                status,
                "cut {ordinal}: {agent}"
            );
            assert_eq!(
                again.attempt(agent).unwrap(),
                reopened.attempt(agent).unwrap()
            );
            assert_eq!(
                again
                    .operations()
                    .values()
                    .filter(|operation| operation.caller_agent_id == agent
                        && operation.tool_name == "seal_effect")
                    .count(),
                1
            );
            assert!(
                again
                    .operations()
                    .values()
                    .filter(|operation| operation.caller_agent_id == agent
                        && operation.tool_name == kind)
                    .all(|operation| operation.terminal_response.as_deref()
                        == Some("effects_reserved"))
            );
        }
        assert!(
            vcs.calls_of(FakeOp::Seal).len() <= 1,
            "cut {ordinal}: duplicate seal dispatch"
        );
    }
    eprintln!("two-save tree commit cuts={}", frames.len());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one deterministic resume commit-cut scenario"
)]
fn transient_resume_decision_commit_cuts_are_conservative_then_replayable() {
    use plakous_recursive_subagents::protocol::AttemptStatus;
    use std::os::unix::process::ExitStatusExt;
    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let store = Arc::new(SqliteTreeStore::open(&root.path().join("source.sqlite3")).unwrap());
    let tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        TREE,
        "root",
        "owner",
        root.path().to_owned(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let driver = TreeDriver::new(Arc::new(Mutex::new(tree)), store, vcs.clone(), None);
    let feeds = Mutex::new(vec![Feed::default(); 2]);
    feeds.lock().unwrap()[0].push(0, Vec::new());
    let root_feed = feeds.lock().unwrap()[0].clone();
    publish(&driver, |tree| {
        project(tree, 0, &root_feed);
        tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
    });
    admit(&driver, &feeds, 1, 1);
    let mut feed = feeds.lock().unwrap()[1].clone();
    feed.reads.insert_failure(
        turn(1, 1).to_string(),
        FailureRead {
            kind: AgentErrorKind::Model,
            retryability: Some(plakous::model::ModelRetryability::Exhausted),
            phase: Some(plakous::model::ModelErrorPhase::Connect),
            http_status: None,
            provider_code: None,
            message: "provider unreachable".into(),
        },
    );
    feed.push(
        1,
        vec![FeedFact::TurnTerminal {
            turn_id: turn(1, 1),
            command_id: CommandId::new("command:1:1").unwrap(),
            outcome: FeedTurnOutcome::Failed {
                kind: AgentErrorKind::Model,
                model_kind: Some(plakous::model::ModelErrorKind::Transport),
                context: None,
            },
            closed_exchange: false,
            steering_settled_unapplied: false,
            usage: TurnUsage::default(),
        }],
    );
    publish(&driver, |tree| {
        let attempt = tree.attempt("c1").unwrap().attempt_id.clone();
        tree.record_failed_turn_notice(
            "c1",
            &attempt,
            &turn(1, 1).to_string(),
            "snapshot notice".into(),
        )
        .unwrap();
    });
    let before = driver.read(Clone::clone).unwrap();
    publish(&driver, |tree| {
        tree.record_child_finalization(
            "c1",
            &turn(1, 1).to_string(),
            ChildFinalizationDecision::Resume { round: 1 },
            false,
            None,
        )
        .unwrap();
    });
    let after = driver.read(Clone::clone).unwrap();
    for (index, frame) in [&before, &after].into_iter().enumerate() {
        write_frame(&root.path().join(format!("frame-{index}.sqlite3")), frame);
    }
    std::fs::write(root.path().join("count"), "2").unwrap();
    let db = root.path().join("cut.sqlite3");
    drop(SqliteTreeStore::open(&db).unwrap());
    let empty = root.path().join("empty.sqlite3");
    std::fs::copy(&db, &empty).unwrap();
    assert!(run_child(root.path(), None).status.success());
    for cut in 1..=2 {
        std::fs::remove_file(root.path().join("trace.jsonl")).unwrap();
        std::fs::copy(&empty, &db).unwrap();
        let child = run_child(root.path(), Some(cut));
        assert_eq!(
            child.status.signal(),
            Some(9),
            "cut {cut}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let store = SqliteTreeStore::open(&db).unwrap();
        let mut reopened = ProductAgentTree::reopen(store.load(TREE).unwrap().unwrap());
        reopened.set_vcs(vcs.clone());
        project(&mut reopened, 1, &feed);
        assert_eq!(
            reopened.attempt("c1").unwrap().status,
            if cut == 1 {
                AttemptStatus::Failed
            } else {
                AttemptStatus::Running
            }
        );
        if cut == 2 {
            assert_eq!(
                reopened.pending_transient_resume("c1").unwrap(),
                Some((turn(1, 1).to_string(), 1))
            );
            assert!(reopened.take_effects().is_empty());
            reopened.acknowledge_revision(store.save(&reopened).unwrap());
            let mut replay = ProductAgentTree::reopen(store.load(TREE).unwrap().unwrap());
            project(&mut replay, 1, &feed);
            assert_eq!(replay.attempt("c1"), reopened.attempt("c1"));
            assert!(replay.take_effects().is_empty());
        }
    }
}
