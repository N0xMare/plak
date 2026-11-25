use plakous::agent::CommandId;
use plakous::model::{Digest, DigestDomain};
use plakous::sessions::feed::FeedRecord;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_hosted::sessions::projector::{
    Delivery, SessionViews, Submission, SubmissionKind, ViewWrites,
};
use plakous_hosted::sessions::{QueueItem, SessionRecord};
use plakous_recursive_subagents_store::SqliteTreeStore;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::home::paths::PlakPaths;
use crate::store::{ProductStore, queue_tombstone_digest};

use super::{Evidence, Snapshot};

mod live;

const SESSION: &str = "00000000-0000-0000-0000-0000000002bc";
const BRANCH: &str = "00000000-0000-0000-0000-0000000002bd";
const OTHER: &str = "00000000-0000-0000-0000-0000000002be";
const PARENT: &str = "00000000-0000-0000-0000-000000000323";
const CHECKPOINT: &str = "00000000-0000-0000-0000-000000000354";
const TURN: &str = "00000000-0000-0000-0000-0000000003e7";
const COMMAND: &str = "fixture.queue.command";
const TEXT: &str = "wake draft";
const ANCHOR: u64 = 5;

struct Fixture {
    temp: tempfile::TempDir,
    paths: PlakPaths,
    product: Connection,
    writer: ProductStore,
    runtime: Connection,
}

impl Fixture {
    fn new() -> Self {
        Self::with_stub_runtime(true)
    }

    fn with_stub_runtime(stub_runtime: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(temp.path().join("home"));
        let writer = ProductStore::open(&paths).unwrap();
        drop(SqliteTreeStore::open(&paths.subagents_database()).unwrap());
        drop(SessionBackend::sqlite(paths.runtime_database()).unwrap());
        let product = Connection::open(paths.product_database()).unwrap();
        let runtime = Connection::open(paths.runtime_database()).unwrap();
        product
            .execute(
                "
            INSERT INTO sessions
              (id,revision,create_operation_id,workspace_root,workspace_identity,created_at,
               updated_at,locator,node_tree_id,node_agent_id,archived)
            VALUES('root',1,'create','workspace','workspace',1,1,?1,'tree','agent',0)
        ",
                [format!("{SESSION}/{BRANCH}/{PARENT}")],
            )
            .unwrap();
        product
            .execute(
                "INSERT INTO queue_items VALUES('root','item',1,0,?1,100)",
                [TEXT],
            )
            .unwrap();
        if stub_runtime {
            runtime
                .execute(
                    "
            INSERT INTO sessions
              (session_id,initial_branch,revision,created_s,created_ns,durability,archived,
               feed_tail,feed_retained_from,feed_bytes)
            VALUES(?1,?2,zeroblob(8),zeroblob(8),0,2,0,0,1,0)
        ",
                    [SESSION, BRANCH],
                )
                .unwrap();
        }
        Self {
            temp,
            paths,
            product,
            writer,
            runtime,
        }
    }

    fn before(&self) -> Snapshot {
        Snapshot::capture(&self.paths, None).unwrap().0
    }

    fn compare(&self, before: &Snapshot) -> crate::CliResult<Option<String>> {
        let (after, accepted) = Snapshot::capture(&self.paths, Some(before))?;
        before.compare(&after, &accepted, &self.paths)
    }

    fn attach(&self) {
        self.product
            .execute(
                "INSERT INTO queue_attachments VALUES('item',0,'image','image/png',x'0102')",
                [],
            )
            .unwrap();
    }

    fn remove(&self, kind: SubmissionKind, target: Option<&str>) {
        let submission = Submission {
            command_id: COMMAND.to_owned(),
            queue_item_id: "item".to_owned(),
            queue_item_revision: 1,
            kind,
            target_turn_id: target.map(str::to_owned),
            canonical_command: format!("{TEXT}\n\n<child_message>finished</child_message>"),
            submitted_at_ms: 100,
        };
        self.product
            .execute_batch(
                "DELETE FROM queue_attachments; DELETE FROM queue_items WHERE item_id='item';",
            )
            .unwrap();
        self.product
            .execute(
                "
            INSERT INTO identity_bindings VALUES('item','queue','root',?1,'deleted',NULL)
        ",
                [queue_tombstone_digest("root", "1", TEXT)],
            )
            .unwrap();
        self.product
            .execute(
                "
            INSERT INTO identity_bindings VALUES(?1,'command','root',?2,NULL,NULL)
        ",
                [
                    submission.binding_identity(),
                    submission.binding_digest("root"),
                ],
            )
            .unwrap();
        self.product
            .execute(
                "
            INSERT INTO submissions VALUES('root',?1,?2,'item',1,?3,?4,100,?5)
        ",
                params![
                    COMMAND,
                    kind.as_str(),
                    target,
                    submission.canonical_command,
                    i64::try_from(ANCHOR).unwrap()
                ],
            )
            .unwrap();
    }

    fn feed(&self, branch: &str, seq: u64, facts: &[Value]) {
        self.feed_bytes(branch, seq, &feed_payload(branch, seq, facts));
    }

    fn feed_bytes(&self, branch: &str, seq: u64, bytes: &[u8]) {
        // Build a sealed fixture in the runtime's v1 receipt format so
        // corrupt payload and header cases exercise more than a bad seal.
        let mut framed = Vec::new();
        for part in [
            b"feed-record-v1".as_slice(),
            SESSION.as_bytes(),
            branch.as_bytes(),
            &seq.to_be_bytes(),
            &2_u64.to_be_bytes(),
            CHECKPOINT.as_bytes(),
            bytes,
        ] {
            framed.extend((part.len() as u64).to_be_bytes());
            framed.extend(part);
        }
        let digest = Digest::sha256(DigestDomain::Blob, 1, &[&framed]).unwrap();
        self.runtime
            .execute(
                "
            INSERT INTO session_feed VALUES(?1,?2,?3,2,?4,?5,?6)
        ",
                params![
                    SESSION,
                    i64::try_from(seq).unwrap(),
                    branch,
                    CHECKPOINT,
                    bytes,
                    digest.bytes()
                ],
            )
            .unwrap();
        self.runtime
            .execute(
                "UPDATE sessions SET feed_retained_from=(SELECT min(seq) FROM session_feed WHERE session_id=?1),
                                 feed_tail=(SELECT max(seq) FROM session_feed WHERE session_id=?1) WHERE session_id=?1",
                [SESSION],
            )
            .unwrap();
    }

    fn refuses(&self, before: &Snapshot, reason: &str) {
        let error = self.compare(before).unwrap_err();
        assert!(error.message().contains(reason), "{}", error.message());
    }
}

fn feed_payload(branch: &str, seq: u64, facts: &[Value]) -> Vec<u8> {
    let record: FeedRecord = serde_json::from_value(json!({
        "header": {"session_id": SESSION, "branch_id": branch, "seq": seq,
            "committed_at_ms":101, "change":{"advanced":{
                "parent": PARENT, "checkpoint": CHECKPOINT, "revision": 4, "head":"quiescent"
            }}},
        "facts": facts,
    }))
    .unwrap();
    record.encode().unwrap()
}

fn accepted(kind: Value, turn: &str) -> Value {
    json!({"command_accepted":{
        "command_id": COMMAND, "kind":kind, "turn_id":turn, "sequence":1, "operation_id":null
    }})
}

fn wake() -> Value {
    json!({"root":{"origin":"host_wake"}})
}

#[tokio::test]
async fn a_sealed_child_wake_passes_while_the_delivery_projection_still_lags() {
    let temp = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(temp.path().join("home"));
    let mut product = ProductStore::open(&paths).unwrap();
    drop(SqliteTreeStore::open(&paths.subagents_database()).unwrap());
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let session = native_session(&sessions, temp.path()).await;
    let mut root = SessionRecord::new(temp.path().to_path_buf());
    root.plakous_locator = Some(session.locator());
    let draft = QueueItem::new(TEXT.to_owned());
    let submission = Submission {
        command_id: COMMAND.to_owned(),
        queue_item_id: draft.id.clone(),
        queue_item_revision: draft.revision,
        kind: SubmissionKind::HostWake,
        target_turn_id: None,
        canonical_command: format!("{TEXT}\n\n<child_message>finished</child_message>"),
        submitted_at_ms: draft.submitted_at_ms,
    };
    root.pending_queue.push(draft);
    product.commit_submission(&mut root, &submission).unwrap();
    let before = Snapshot::capture(&paths, None).unwrap().0;
    session
        .agent()
        .start_instruction(
            CommandId::new(COMMAND).unwrap(),
            submission.canonical_command,
        )
        .await
        .unwrap()
        .into_turn()
        .unwrap()
        .result()
        .await
        .unwrap();
    root.pending_queue.clear();
    product.commit(&mut root).unwrap();
    assert!(product.load_views(&root.id).unwrap().is_none());
    let (after, accepted) = Snapshot::capture(&paths, Some(&before)).unwrap();
    before.compare(&after, &accepted, &paths).unwrap();
    session.release().await.unwrap();
}

async fn native_session(
    sessions: &AgentSessions,
    workspace: &std::path::Path,
) -> plakous::sessions::OpenedSession {
    native_session_with_id(sessions, workspace, "preservation:fixture").await
}

async fn native_session_with_id(
    sessions: &AgentSessions,
    workspace: &std::path::Path,
    id: &str,
) -> plakous::sessions::OpenedSession {
    use plakous_offline_crash_harness::{
        BoundaryController, protocol::CrashBoundary, text_runtime_binding,
    };
    let binding = text_runtime_binding(
        workspace,
        BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            workspace.join("evidence"),
        )
        .unwrap(),
        "synthetic",
        "synthetic",
    )
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new(id).unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("runtime fixture was not created");
    };
    session
}

#[tokio::test]
async fn native_pruning_preserves_a_departed_head_only_with_its_sealed_horizon() {
    let temp = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(temp.path().join("home"));
    drop(ProductStore::open(&paths).unwrap());
    drop(SqliteTreeStore::open(&paths.subagents_database()).unwrap());
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let session = native_session(&sessions, temp.path()).await;
    let turn = |n| CommandId::new(format!("preservation:turn:{n}")).unwrap();
    session
        .agent()
        .start(turn(0), "first")
        .await
        .unwrap()
        .into_turn()
        .unwrap()
        .result()
        .await
        .unwrap();
    let former = session.locator();
    let before = Snapshot::capture(&paths, None).unwrap().0;
    // The runtime pins the four most recent terminal checkpoints.
    for n in 1..6 {
        session
            .agent()
            .start(turn(n), "next")
            .await
            .unwrap()
            .into_turn()
            .unwrap()
            .result()
            .await
            .unwrap();
    }
    let head = session.locator();
    session.release().await.unwrap();
    sessions.prune_history(head).await.unwrap();
    let runtime = Connection::open(paths.runtime_database()).unwrap();
    let pruned: bool = runtime
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pruned_checkpoints WHERE checkpoint_id=?1)",
            [former.checkpoint_id().to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        pruned,
        "test needs the pre-handoff head to be physically pruned"
    );
    let (after, accepted) = Snapshot::capture(&paths, Some(&before)).unwrap();
    before.compare(&after, &accepted, &paths).unwrap();
    runtime
        .execute("UPDATE branch_horizons SET integrity=zeroblob(32)", [])
        .unwrap();
    let Err(error) = Snapshot::capture(&paths, Some(&before)) else {
        panic!("unsealed prune horizon was accepted");
    };
    assert!(error.message().contains("sealed branch horizon"));
}

#[test]
fn root_start_and_steer_need_the_correct_command_class_and_turn() {
    for (kind, target, fact) in [
        (
            SubmissionKind::RootStart,
            None,
            json!({"root":{"origin":"user_prompt"}}),
        ),
        (SubmissionKind::SendNowSteer, Some(TURN), json!("steer")),
    ] {
        let f = Fixture::new();
        let before = f.before();
        f.remove(kind, target);
        f.feed(BRANCH, ANCHOR + 1, &[accepted(fact, TURN)]);
        f.compare(&before).unwrap();
    }
    for (kind, target, fact) in [
        (SubmissionKind::RootStart, None, wake()),
        (SubmissionKind::SendNowSteer, Some(OTHER), json!("steer")),
    ] {
        let f = Fixture::new();
        let before = f.before();
        f.remove(kind, target);
        f.feed(BRANCH, ANCHOR + 1, &[accepted(fact, TURN)]);
        f.refuses(&before, "admission that disagrees");
    }
}

#[test]
fn missing_rejected_stale_and_off_branch_admissions_refuse_even_with_a_delivery_view() {
    let mut f = Fixture::new();
    let before = f.before();
    f.remove(SubmissionKind::HostWake, None);
    let empty = SessionViews::empty();
    let mut projected = empty.clone();
    projected
        .state
        .deliveries
        .insert(COMMAND.to_owned(), Delivery::Accepted);
    f.writer
        .commit_projection_views(
            "root",
            &ViewWrites::between(&empty, &projected),
            &projected.state,
        )
        .unwrap();
    f.refuses(&before, "without exact durable admission");
    f.feed(BRANCH, ANCHOR, &[accepted(wake(), TURN)]);
    f.refuses(&before, "without exact durable admission");
    f.feed(OTHER, ANCHOR + 1, &[accepted(wake(), TURN)]);
    f.refuses(&before, "without exact durable admission");
    f.feed(
        BRANCH,
        ANCHOR + 2,
        &[json!({"command_rejected": {
            "command_id":COMMAND,"kind":wake(),"error":"wrong_phase"
        }})],
    );
    f.refuses(&before, "without exact durable admission");
}

#[test]
fn corrupt_feed_rows_and_forged_submission_bindings_refuse() {
    let f = Fixture::new();
    let before = f.before();
    f.remove(SubmissionKind::HostWake, None);
    f.feed_bytes(BRANCH, ANCHOR + 1, b"{\"version\":1,");
    f.refuses(&before, "sealed runtime feed");
    f.runtime.execute("DELETE FROM session_feed", []).unwrap();
    let mut payload = feed_payload(BRANCH, ANCHOR + 1, &[accepted(wake(), TURN)]);
    assert!(payload.ends_with(b"]}}"));
    payload.truncate(payload.len() - 3);
    payload.extend_from_slice(b",null]}}");
    f.feed_bytes(BRANCH, ANCHOR + 1, &payload);
    f.refuses(&before, "sealed runtime feed");
    f.runtime.execute("DELETE FROM session_feed", []).unwrap();
    f.feed(BRANCH, ANCHOR + 1, &[accepted(wake(), TURN)]);
    f.runtime
        .execute("UPDATE session_feed SET integrity=zeroblob(32)", [])
        .unwrap();
    f.refuses(&before, "sealed runtime feed");
    f.product
        .execute("UPDATE submissions SET canonical_command='forged'", [])
        .unwrap();
    f.refuses(&before, "permanent command binding");
}

#[test]
fn removed_attachments_and_edited_or_wrong_owner_tombstones_are_not_admitted() {
    for mutation in [
        "UPDATE identity_bindings SET binding_digest='wrong' WHERE identity='item'",
        "UPDATE identity_bindings SET owner_session_id='other' WHERE identity='item'",
        "UPDATE submissions SET queue_revision=2",
    ] {
        let f = Fixture::new();
        let before = f.before();
        f.remove(SubmissionKind::HostWake, None);
        f.product.execute(mutation, []).unwrap();
        f.feed(BRANCH, ANCHOR + 1, &[accepted(wake(), TURN)]);
        f.refuses(&before, "without exact durable admission");
    }
    let f = Fixture::new();
    f.attach();
    let before = f.before();
    f.remove(SubmissionKind::HostWake, None);
    f.feed(BRANCH, ANCHOR + 1, &[accepted(wake(), TURN)]);
    f.refuses(&before, "without exact durable admission");
}

#[test]
fn queue_append_is_normal_but_edits_reorders_and_attachment_loss_refuse() {
    let f = Fixture::new();
    f.attach();
    let before = f.before();
    f.product
        .execute(
            "INSERT INTO queue_items VALUES('root','other',1,1,'next',100)",
            [],
        )
        .unwrap();
    f.compare(&before).unwrap();
    let before = f.before();
    f.product
        .execute_batch(
            "
        UPDATE queue_items SET position=2 WHERE item_id='item';
        UPDATE queue_items SET position=0 WHERE item_id='other';
    ",
        )
        .unwrap();
    f.refuses(&before, "queued prefix");
    f.product
        .execute_batch(
            "
        UPDATE queue_items SET position=1 WHERE item_id='other';
        UPDATE queue_items SET position=0 WHERE item_id='item';
        UPDATE queue_items SET text='edited' WHERE item_id='item';
    ",
        )
        .unwrap();
    f.refuses(&before, "queued prefix");
    f.product
        .execute(
            "UPDATE queue_items SET text=?1 WHERE item_id='item'",
            [TEXT],
        )
        .unwrap();
    f.product
        .execute("DELETE FROM queue_attachments", [])
        .unwrap();
    f.refuses(&before, "queued prefix");
}

#[test]
fn retirement_enabled_home_refuses_before_store_reads() {
    let f = Fixture::new();
    std::fs::write(
        f.paths.config_file(),
        "[runtime_store]\nretire_finished_children = true\n",
    )
    .unwrap();
    let Err(error) = Snapshot::capture(&f.paths, None) else {
        panic!("retirement-enabled home did not refuse");
    };
    assert!(
        error
            .message()
            .contains("cannot verify intentional retirement")
    );
}

#[test]
fn tree_records_keep_once_set_facts_prefixes_and_fences() {
    let f = Fixture::new();
    let tree = Connection::open(f.paths.subagents_database()).unwrap();
    tree.execute_batch("
        INSERT INTO agent_trees VALUES('tree','agent',1,1,'live','single_lease','selection','checkout','{}',0,NULL);
        INSERT INTO agent_commands VALUES('tree','command','agent',NULL,'wake','text','[]',0,NULL,NULL,NULL);
        INSERT INTO agent_tree_runner_ancestors VALUES('tree','runner','[\"first\"]');
        INSERT INTO agent_tree_custody VALUES('tree',1,'owner','instance',NULL,'label',0,0,0);
    ").unwrap();
    let before = f.before();
    tree.execute_batch(
        "
        UPDATE agent_trees SET current_epoch=1,revision=2,host_generation=2;
        UPDATE agent_commands SET fact='accepted',turn_id='turn',accepted_seq=2;
        UPDATE agent_tree_runner_ancestors SET commits='[\"first\",\"next\"]';
        UPDATE agent_tree_custody SET fence=2,host_instance='successor';
    ",
    )
    .unwrap();
    f.compare(&before).unwrap();
    let before = f.before();
    tree.execute("UPDATE agent_commands SET turn_id='another'", [])
        .unwrap();
    f.refuses(&before, "agent_commands");
    tree.execute("UPDATE agent_commands SET turn_id='turn'", [])
        .unwrap();
    tree.execute(
        "UPDATE agent_tree_runner_ancestors SET commits='[\"next\",\"first\"]'",
        [],
    )
    .unwrap();
    f.refuses(&before, "agent_tree_runner_ancestors");
    tree.execute(
        "UPDATE agent_tree_runner_ancestors SET commits='[\"first\",\"next\"]'",
        [],
    )
    .unwrap();
    tree.execute("UPDATE agent_tree_custody SET fence=1", [])
        .unwrap();
    f.refuses(&before, "tree fence");
    tree.execute("UPDATE agent_tree_custody SET fence=2", [])
        .unwrap();
    tree.execute("UPDATE agent_trees SET current_epoch=0", [])
        .unwrap();
    f.refuses(&before, "agent_trees");
}

#[test]
fn tree_revision_needs_a_new_intact_stopped_copy_of_the_same_store() {
    use plakous_recursive_subagents_store::database::schema;
    let f = Fixture::new();
    let path = f.paths.subagents_database();
    let schema = schema();
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "DROP TABLE agent_child_archive_receipts; DROP TABLE agent_retired_children; PRAGMA user_version=11;",
        )
        .unwrap();
    legacy
        .execute(
            "UPDATE subagents_schema SET fingerprint=?1",
            [schema.base_fingerprint],
        )
        .unwrap();
    drop(legacy);
    let before = f.before();
    schema
        .lock_upgrade(&path)
        .unwrap()
        .unwrap()
        .backup()
        .unwrap()
        .commit(|_, _| {})
        .unwrap();
    let backup = f.compare(&before).unwrap().unwrap();
    assert!(crate::shipping_artifact::hex(&backup, 64));
    std::fs::remove_file(path.with_file_name("subagents.pre-revision-11.sqlite3")).unwrap();
    f.refuses(&before, "stopped-tree backup");
}
