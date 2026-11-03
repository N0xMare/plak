//! Row-level saves leave exactly the rows a whole-tree replace leaves.
//!
//! The reference store below is the whole-tree algorithm itself: fence the
//! revision, delete every row of the tree and insert every row again. The
//! store under test diffs against its last acknowledged save. Generated
//! mutation sequences drive both, including removals, same-revision saves,
//! stale handles, refused saves, a deleted and recreated tree, request keys
//! reissued to a new attempt, a reopened store and a second connection
//! writing the same tree, and every table must match row for row and column
//! for column after every step.
#![cfg(feature = "test-support")]
#![allow(clippy::unwrap_used, reason = "deterministic offline law fixtures")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous_recursive_subagents::epoch::OrchestrationSnapshot;
use plakous_recursive_subagents::protocol::{
    AttemptId, AttemptStatus, CandidateId, MessagePriority, MessagePurpose, NodeStatus,
    SpawnAgentRequest, SpawnWorkspace, Tier, TopologyGrant, TreeAdmissionState, WriterMode,
};
use plakous_recursive_subagents::tree::{
    AgentCommand, AgentCommandKind, AttemptRecord, CandidateRecord, CommandFact, FakeVcs,
    FoldAnchor, ForkedFrom, MailDisposition, MailDurableState, MailRecord, NodeLineId,
    NodePlacement, NodeRecord, NodeWorkspace, OperationRecord, PersistedTree, ProductAgentTree,
    TreeEpochs, TreeStore, TreeStoreError, TreeStoreErrorKind, WorkspaceRecord, fence_revision,
};
use plakous_recursive_subagents_store::{SqliteTreeStore, rows};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use rusqlite::types::Value;

const TREE: &str = "row-law-tree";
const ROOT: &str = "root";

/// Every tree table and its key, in a fixed order.
const TABLES: &[(&str, &str)] = &[
    ("agent_trees", "tree_id"),
    ("agent_nodes", "agent_id"),
    ("agent_grants", "agent_id"),
    ("agent_attempts", "attempt_id"),
    ("agent_candidates", "candidate_id"),
    ("agent_workspaces", "workspace_id"),
    ("agent_messages", "message_id"),
    ("agent_operations", "operation_id"),
    ("agent_commands", "command_id"),
    ("agent_feed_cursors", "runtime_session_id"),
    ("agent_tree_epochs", "epoch_no"),
    ("agent_tree_runner_ancestors", "runner_id"),
];

/// Every column of every row of one tree, ordered by key.
fn dump(path: &Path, tree_id: &str) -> Vec<(String, Vec<Vec<Value>>)> {
    let connection = rusqlite::Connection::open(path).unwrap();
    TABLES
        .iter()
        .map(|(table, key)| {
            let mut statement = connection
                .prepare(&format!(
                    "SELECT * FROM {table} WHERE tree_id=?1 ORDER BY {key}"
                ))
                .unwrap();
            let width = statement.column_count();
            let rows = statement
                .query_map([tree_id], |row| {
                    (0..width).map(|index| row.get::<_, Value>(index)).collect()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            ((*table).to_owned(), rows)
        })
        .collect()
}

/// The whole-tree save, as the store wrote every save before row-level
/// writes: one `Immediate` transaction, the revision fence, then delete and
/// reinsert every row.
struct WholeTree(rusqlite::Connection);

impl WholeTree {
    fn open(path: &Path) -> Self {
        drop(SqliteTreeStore::open(path).unwrap());
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
            .unwrap();
        Self(connection)
    }

    fn save(&mut self, tree: &ProductAgentTree) -> Result<(), TreeStoreError> {
        let tx = self
            .0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let stored = tx
            .query_row(
                "SELECT revision FROM agent_trees WHERE tree_id=?1",
                [tree.tree_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|revision| u64::try_from(revision).unwrap());
        fence_revision(stored, tree.stored_revision, tree.revision)?;
        rows::delete_agent_tree_rows(&tx, &tree.tree_id)?;
        rows::insert_agent_tree_rows(&tx, tree)?;
        tx.commit().unwrap();
        Ok(())
    }

    /// The store's delete: fenced on the inspected revision.
    fn delete(&mut self, tree_id: &str, expected: u64) {
        let tx = self
            .0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let stored: i64 = tx
            .query_row(
                "SELECT revision FROM agent_trees WHERE tree_id=?1",
                [tree_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(u64::try_from(stored).unwrap(), expected);
        rows::delete_agent_tree_rows(&tx, tree_id).unwrap();
        tx.commit().unwrap();
    }
}

/// Record shapes taken from a real spawned child, so the generated rows
/// carry production field values rather than defaults.
struct Templates {
    tree: ProductAgentTree,
    node: NodeRecord,
    grant: TopologyGrant,
    attempt: AttemptRecord,
    keyed_attempt: AttemptRecord,
    workspace: WorkspaceRecord,
}

impl Templates {
    fn new(root: &Path) -> Self {
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            Arc::new(FakeVcs::new()),
            TREE,
            ROOT,
            "owner",
            root.to_owned(),
            "tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut request =
            SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "task");
        request.workspace = SpawnWorkspace::Fork;
        let spawned = tree.spawn_agent(ROOT, &request, None).unwrap();
        let node = tree.nodes()[&spawned.agent_id].clone();
        let grant = tree.grants()[&spawned.agent_id].clone();
        let attempt = tree.attempts()[&spawned.attempt_id].clone();
        let workspace = tree.workspaces()[&node.workspace_id].clone();
        let mut keyed = SpawnAgentRequest::test_named("implement", Tier::Mutate, "keyed", "task");
        keyed.request_key = Some("template-key".to_owned());
        let keyed = tree.spawn_agent(ROOT, &keyed, None).unwrap();
        let keyed_attempt = tree.attempts()[&keyed.attempt_id].clone();
        assert!(keyed_attempt.request_key.is_some() && keyed_attempt.spawn_identity.is_some());
        Self {
            tree,
            node,
            grant,
            attempt,
            keyed_attempt,
            workspace,
        }
    }
}

fn text(label: &str, seed: u32) -> String {
    format!("{label}-{seed}-{}", "x".repeat((seed % 97) as usize))
}

fn request_key(slot: u8) -> String {
    format!("key-{slot}")
}

/// One canonical line id per node slot: a line maps to one node.
fn line_id(slot: u8) -> NodeLineId {
    NodeLineId::parse(&format!(
        "00000000-0000-4000-8000-{:012x}",
        u32::from(slot) + 1
    ))
    .unwrap()
}

fn node_id(slot: u8) -> String {
    if slot == 0 {
        ROOT.to_owned()
    } else {
        format!("n{slot}")
    }
}

/// The persisted maps and header the generated sequence edits.
#[derive(Clone)]
struct Model {
    revision: u64,
    host_generation: u64,
    state: TreeAdmissionState,
    selected_tuple: String,
    nodes: BTreeMap<String, NodeRecord>,
    grants: BTreeMap<String, TopologyGrant>,
    attempts: BTreeMap<AttemptId, AttemptRecord>,
    candidates: BTreeMap<CandidateId, CandidateRecord>,
    workspaces: BTreeMap<String, WorkspaceRecord>,
    messages: BTreeMap<String, MailRecord>,
    operations: BTreeMap<String, OperationRecord>,
    commands: BTreeMap<String, AgentCommand>,
    cursors: BTreeMap<String, u64>,
    forked_from: Option<ForkedFrom>,
    epochs: BTreeMap<u64, OrchestrationSnapshot>,
    ancestors: BTreeMap<String, BTreeSet<String>>,
}

impl Model {
    fn new(templates: &Templates) -> Self {
        let mut root = templates.node.clone();
        ROOT.clone_into(&mut root.agent_id);
        root.parent_agent_id = None;
        root.depth = 0;
        Self {
            revision: 1,
            host_generation: 1,
            state: TreeAdmissionState::Live,
            selected_tuple: "tuple".to_owned(),
            nodes: BTreeMap::from([(ROOT.to_owned(), root)]),
            grants: BTreeMap::new(),
            attempts: BTreeMap::new(),
            candidates: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            messages: BTreeMap::new(),
            operations: BTreeMap::new(),
            commands: BTreeMap::new(),
            cursors: BTreeMap::new(),
            forked_from: None,
            epochs: BTreeMap::new(),
            ancestors: BTreeMap::new(),
        }
    }

    #[allow(clippy::too_many_lines, reason = "one generated row per table")]
    fn put(&mut self, templates: &Templates, table: u8, slot: u8, seed: u32) {
        match table {
            0 => {
                let mut node = templates.node.clone();
                node.agent_id = node_id(slot);
                node.parent_agent_id = (slot != 0).then(|| ROOT.to_owned());
                node.depth = u8::from(slot != 0);
                node.prompt = text("prompt", seed);
                node.status = [
                    NodeStatus::Running,
                    NodeStatus::Completed,
                    NodeStatus::Failed,
                ][(seed % 3) as usize];
                node.active_turn = seed.is_multiple_of(2);
                node.fold_order = (!seed.is_multiple_of(3)).then_some(u64::from(seed % 7));
                node.session_locator = seed.is_multiple_of(4).then(|| text("locator", seed));
                node.line_id = seed.is_multiple_of(2).then(|| line_id(slot));
                node.placement = if seed.is_multiple_of(3) {
                    NodePlacement {
                        runner_id: Some(text("runner", seed)),
                        workspace: Some(NodeWorkspace {
                            root: PathBuf::from(text("root", seed)),
                            cwd: PathBuf::from(text("cwd", seed)),
                        }),
                    }
                } else {
                    NodePlacement::default()
                };
                node.parent_composition = seed
                    .is_multiple_of(5)
                    .then(|| serde_json::from_value(serde_json::json!("js-ptc")).unwrap());
                self.nodes.insert(node.agent_id.clone(), node);
            }
            1 => {
                let id = node_id(slot);
                if !self.nodes.contains_key(&id) {
                    return;
                }
                let mut grant = templates.grant.clone();
                grant.visible_ids = (0..seed % 4).map(|index| format!("n{index}")).collect();
                grant.may_spawn = seed.is_multiple_of(2);
                grant.max_depth = (seed % 5) as u8;
                grant.max_nodes = 1 + (seed % 65) as u8;
                grant.max_children = 1 + (seed % 64) as u8;
                self.grants.insert(id, grant);
            }
            2 => {
                let owners = self.nodes.keys().cloned().collect::<Vec<_>>();
                if owners.is_empty() {
                    return;
                }
                let owner = owners[seed as usize % owners.len()].clone();
                let keyed = seed.is_multiple_of(3);
                let mut attempt = if keyed {
                    templates.keyed_attempt.clone()
                } else {
                    templates.attempt.clone()
                };
                attempt.attempt_id = AttemptId::from_persisted(format!("a{slot}"));
                if keyed {
                    attempt.request_key = Some(request_key(slot));
                    // One holder per key: a keyed spawn retires any other
                    // attempt holding it.
                    self.attempts.retain(|id, other| {
                        *id == attempt.attempt_id || other.request_key != attempt.request_key
                    });
                }
                attempt.agent_id = owner;
                attempt.status = [
                    AttemptStatus::Running,
                    AttemptStatus::Completed,
                    AttemptStatus::Failed,
                ][(seed % 3) as usize];
                attempt.turn_token = 1 + u64::from(seed % 9);
                attempt.output_json = seed.is_multiple_of(2).then(|| text("output", seed));
                attempt.usage.turns = seed % 11;
                attempt.usage.input_tokens = u64::from(seed);
                attempt.ended_at = seed.is_multiple_of(3).then_some(u64::from(seed));
                attempt.applied_terminal_read_seqs = (0..u64::from(seed % 3)).collect();
                self.attempts.insert(attempt.attempt_id.clone(), attempt);
            }
            3 => {
                let id = CandidateId::from_persisted(format!("cand-{slot}"));
                self.candidates.insert(
                    id.clone(),
                    CandidateRecord {
                        reverted_at: None,
                        reverted_by: None,
                        kind: plakous_recursive_subagents::tree::CandidateKind::Work,
                        source_candidate_id: None,
                        conflict_target: None,
                        conflict_paths: Vec::new(),
                        resolves: None,
                        adopted_via: None,
                        target_before: seed.is_multiple_of(2).then(|| text("before", seed)),
                        candidate_id: id,
                        workspace_id: format!("w{slot}"),
                        producing_agent_id: node_id(slot),
                        producing_attempt_id: AttemptId::from_persisted(format!("a{slot}")),
                        base_commit: text("base", seed),
                        base_content: text("content", seed),
                        base_change_id: seed.is_multiple_of(2).then(|| text("change", seed)),
                        commit: text("commit", seed),
                        content: text("tree", seed),
                        change_id: None,
                        sealed_at: u64::from(seed),
                        adopted_by: seed.is_multiple_of(3).then(|| ROOT.to_owned()),
                        merged_at: None,
                        adopted_evidence: (0..seed % 3).map(|index| format!("e{index}")).collect(),
                    },
                );
            }
            4 => {
                let mut workspace = templates.workspace.clone();
                workspace.workspace_id = format!("w{slot}");
                workspace.owner_agent_id = node_id(slot);
                workspace.path = PathBuf::from(text("copy", seed));
                workspace.generation = u64::from(seed % 5);
                workspace.base_commit = seed.is_multiple_of(2).then(|| text("base", seed));
                workspace.input_candidate_id = seed
                    .is_multiple_of(3)
                    .then(|| CandidateId::from_persisted(text("input", seed)));
                self.workspaces
                    .insert(workspace.workspace_id.clone(), workspace);
            }
            5 => {
                let id = format!("m{}", u32::from(slot) + 1);
                self.messages.insert(
                    id.clone(),
                    MailRecord {
                        message_id: id,
                        from: ROOT.to_owned(),
                        to: node_id(slot),
                        body: text("body", seed),
                        purpose: [
                            MessagePurpose::Coordinate,
                            MessagePurpose::Followup,
                            MessagePurpose::Result,
                        ][(seed % 3) as usize],
                        priority: [MessagePriority::Deferred, MessagePriority::Urgent]
                            [(seed % 2) as usize],
                        in_reply_to: seed.is_multiple_of(4).then(|| "m1".to_owned()),
                        disposition: [MailDisposition::Started, MailDisposition::Queued]
                            [(seed % 2) as usize],
                        durable: [
                            MailDurableState::Accepted,
                            MailDurableState::Acknowledged,
                            MailDurableState::Terminal,
                        ][(seed % 3) as usize],
                    },
                );
            }
            6 => {
                let id = format!("op{slot}");
                self.operations.insert(
                    id.clone(),
                    OperationRecord {
                        operation_id: id,
                        caller_agent_id: node_id(slot),
                        tool_name: "send_agent_message".to_owned(),
                        canonical_request: text("request", seed),
                        request_digest: text("digest", seed),
                        fold: seed.is_multiple_of(2).then_some(FoldAnchor {
                            seq: u64::from(seed % 10),
                            order: 1 + u64::from(seed % 5),
                        }),
                        terminal_code: None,
                        terminal_response: seed.is_multiple_of(3).then(|| text("response", seed)),
                    },
                );
            }
            7 => {
                let mut command = AgentCommand::intent(
                    format!("cmd{slot}"),
                    node_id(slot),
                    seed.is_multiple_of(2)
                        .then(|| AttemptId::from_persisted(format!("a{slot}"))),
                    [AgentCommandKind::Start, AgentCommandKind::Wake][(seed % 2) as usize],
                    text("command", seed),
                    (0..seed % 3)
                        .map(|index| format!("m{}", index + 1))
                        .collect(),
                );
                command.withdrawn = seed.is_multiple_of(5);
                command.fact = [
                    None,
                    Some(CommandFact::Accepted),
                    Some(CommandFact::Returned),
                ][(seed % 3) as usize];
                command.turn_id = command.fact.and(Some(text("turn", seed)));
                command.accepted_seq = (seed % 2 == 1).then_some(1 + u64::from(seed % 9));
                self.commands.insert(command.command_id.clone(), command);
            }
            8 => {
                self.cursors
                    .insert(format!("session-{slot}"), u64::from(seed));
            }
            9 => {
                let mut snapshot = templates.tree.snapshot.clone();
                snapshot.transient_resumes = (seed % 3) as u8;
                snapshot.digest = snapshot.compute_digest();
                self.epochs.insert(1 + u64::from(slot), snapshot);
            }
            _ => {
                self.ancestors.insert(
                    format!("runner-{slot}"),
                    (0..seed % 4).map(|index| text("commit", index)).collect(),
                );
            }
        }
    }

    fn remove(&mut self, table: u8, slot: u8) {
        match table {
            0 => {
                // A node goes with the rows that name it under a foreign key.
                let id = node_id(slot);
                self.nodes.remove(&id);
                self.grants.remove(&id);
                self.attempts.retain(|_, attempt| attempt.agent_id != id);
            }
            1 => {
                self.grants.remove(&node_id(slot));
            }
            2 => {
                for prefix in ["a", "b"] {
                    self.attempts
                        .remove(&AttemptId::from_persisted(format!("{prefix}{slot}")));
                }
            }
            3 => {
                self.candidates
                    .remove(&CandidateId::from_persisted(format!("cand-{slot}")));
            }
            4 => {
                self.workspaces.remove(&format!("w{slot}"));
            }
            5 => {
                self.messages.remove(&format!("m{}", u32::from(slot) + 1));
            }
            6 => {
                self.operations.remove(&format!("op{slot}"));
            }
            7 => {
                self.commands.remove(&format!("cmd{slot}"));
            }
            8 => {
                self.cursors.remove(&format!("session-{slot}"));
            }
            9 => {
                self.epochs.remove(&(1 + u64::from(slot)));
            }
            _ => {
                self.ancestors.remove(&format!("runner-{slot}"));
            }
        }
    }

    /// Removes the attempt holding `slot`'s request key and gives the key to
    /// a new attempt under the other id, in one save: the row path must
    /// delete the old holder before it inserts the new one.
    fn reissue(&mut self, slot: u8, seed: u32) {
        let key = Some(request_key(slot));
        let Some(id) = self
            .attempts
            .iter()
            .find(|(_, attempt)| attempt.request_key == key)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let mut attempt = self.attempts.remove(&id).unwrap();
        let next = if id.as_str().starts_with('a') {
            "b"
        } else {
            "a"
        };
        attempt.attempt_id = AttemptId::from_persisted(format!("{next}{slot}"));
        attempt.usage.tool_calls = seed % 13;
        self.attempts
            .retain(|other, _| *other != attempt.attempt_id);
        self.attempts.insert(attempt.attempt_id.clone(), attempt);
    }

    fn header(&mut self, seed: u32) {
        self.state = [
            TreeAdmissionState::Live,
            TreeAdmissionState::Interrupted,
            TreeAdmissionState::RestoredPaused,
        ][(seed % 3) as usize];
        self.host_generation = 1 + u64::from(seed % 5);
        self.selected_tuple = format!("tuple-{}", seed % 3);
        self.forked_from = match seed % 3 {
            0 => None,
            1 => Some(ForkedFrom::Node {
                tree_id: text("source-tree", seed),
                agent_id: ROOT.to_owned(),
                line_id: line_id(0),
                branch_id: line_id(9).to_string(),
                epoch_no: u64::from(seed % 2),
            }),
            _ => Some(ForkedFrom::Import {
                provenance: text("imported", seed),
            }),
        };
    }

    fn tree(&self, templates: &Templates, tree_id: &str, stored_revision: u64) -> ProductAgentTree {
        let mut operations = self.operations.clone();
        for attempt in self.attempts.values() {
            if let Some(output) = &attempt.output_json {
                let id = format!("submitted:{}", attempt.attempt_id);
                let canonical = serde_json::json!({
                    "attempt_id": attempt.attempt_id,
                    "decision": {"Submitted": {"output_json": output}},
                })
                .to_string();
                operations.insert(
                    id.clone(),
                    OperationRecord {
                        fold: Some(FoldAnchor { seq: 0, order: 1 }),
                        operation_id: id,
                        caller_agent_id: attempt.agent_id.clone(),
                        tool_name: "submitted".into(),
                        request_digest: plakous::model::sha256_hex(&[canonical.as_bytes()]),
                        canonical_request: canonical,
                        terminal_code: None,
                        terminal_response: Some("effects_reserved".into()),
                    },
                );
            }
        }
        let mut tree = PersistedTree::from_rows(
            tree_id.to_owned(),
            ROOT.to_owned(),
            self.revision,
            self.host_generation,
            self.state,
            WriterMode::SingleLease,
            self.selected_tuple.clone(),
            templates.tree.launch_checkout.clone(),
            templates.tree.snapshot.clone(),
            self.nodes.clone(),
            self.grants.clone(),
            self.attempts.clone(),
            self.candidates.clone(),
            self.workspaces.clone(),
            self.messages.clone(),
            operations,
        )
        .with_feed_rows(self.commands.clone(), self.cursors.clone())
        .with_lineage_rows(
            TreeEpochs {
                current: 0,
                successors: self.epochs.clone(),
            },
            self.forked_from.clone(),
            self.ancestors.clone(),
        )
        .rows()
        .clone();
        tree.stored_revision = stored_revision;
        tree
    }
}

#[derive(Clone, Debug)]
enum Step {
    /// Insert or change one row of one table.
    Put { table: u8, slot: u8, seed: u32 },
    /// Remove one key of one table.
    Remove { table: u8, slot: u8 },
    /// Change the header's non-revision columns.
    Header { seed: u32 },
    /// Publish, with or without a revision bump (a receipt lands without).
    Save { bump: bool },
    /// Publish from a handle whose stored revision is wrong.
    Stale { ahead: bool },
    /// Publish a tree whose extra attempt names no node: both stores refuse.
    Broken,
    /// Give a slot's request key to a new attempt (see `Model::reissue`).
    Reissue { slot: u8, seed: u32 },
    /// Delete the tree; the next save recreates it from stored revision 0.
    Delete,
    /// Drop and reopen the row-level store: its next save has no image.
    Reopen,
    /// Another connection publishes the same tree at the same revision.
    Peer { seed: u32 },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (0..11_u8, 0..5_u8, any::<u32>()).prop_map(|(table, slot, seed)| Step::Put { table, slot, seed }),
        2 => (0..11_u8, 0..5_u8).prop_map(|(table, slot)| Step::Remove { table, slot }),
        1 => any::<u32>().prop_map(|seed| Step::Header { seed }),
        4 => any::<bool>().prop_map(|bump| Step::Save { bump }),
        1 => any::<bool>().prop_map(|ahead| Step::Stale { ahead }),
        1 => Just(Step::Broken),
        2 => (0..5_u8, any::<u32>()).prop_map(|(slot, seed)| Step::Reissue { slot, seed }),
        1 => Just(Step::Delete),
        1 => Just(Step::Reopen),
        1 => any::<u32>().prop_map(|seed| Step::Peer { seed }),
    ]
}

struct Harness {
    root: tempfile::TempDir,
    templates: Templates,
    model: Model,
    acknowledged: u64,
    store: SqliteTreeStore,
    reference: WholeTree,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let templates = Templates::new(root.path());
        let model = Model::new(&templates);
        let store = SqliteTreeStore::open(&root.path().join("rows.sqlite3")).unwrap();
        let reference = WholeTree::open(&root.path().join("whole.sqlite3"));
        Self {
            root,
            templates,
            model,
            acknowledged: 0,
            store,
            reference,
        }
    }

    fn rows_path(&self) -> PathBuf {
        self.root.path().join("rows.sqlite3")
    }

    /// Offers one tree to both stores and requires the same answer.
    fn publish(&mut self, tree: &ProductAgentTree) -> Result<(), TreeStoreError> {
        let row_level = self.store.save(tree).map(|_| ());
        let whole = self.reference.save(tree);
        match (&row_level, &whole) {
            (Ok(()), Ok(())) => {}
            (Err(left), Err(right)) => {
                if let TreeStoreErrorKind::StaleRevision { .. } = right.kind {
                    assert_eq!(left, right, "a stale handle must fail as it always did");
                }
            }
            _ => panic!("row-level {row_level:?} but whole-tree {whole:?}"),
        }
        row_level
    }

    fn run(&mut self, step: &Step) {
        match step {
            Step::Put { table, slot, seed } => {
                self.model.put(&self.templates, *table, *slot, *seed);
            }
            Step::Remove { table, slot } => self.model.remove(*table, *slot),
            Step::Header { seed } => self.model.header(*seed),
            Step::Save { bump } => {
                let mut next = self.model.clone();
                if *bump || self.acknowledged == 0 {
                    next.revision = self.acknowledged + 1;
                }
                let tree = next.tree(&self.templates, TREE, self.acknowledged);
                self.publish(&tree).unwrap();
                self.model = next;
                self.acknowledged = self.model.revision;
            }
            Step::Stale { ahead } => {
                if self.acknowledged == 0 {
                    return;
                }
                let wrong = if *ahead {
                    self.acknowledged + 1
                } else {
                    self.acknowledged - 1
                };
                let mut next = self.model.clone();
                next.revision = self.acknowledged + 2;
                let error = self
                    .publish(&next.tree(&self.templates, TREE, wrong))
                    .unwrap_err();
                assert_eq!(
                    error,
                    TreeStoreError::stale(self.acknowledged, next.revision)
                );
            }
            Step::Broken => {
                let mut broken = self.model.clone();
                let mut attempt = self.templates.attempt.clone();
                attempt.attempt_id = AttemptId::from_persisted("orphan".to_owned());
                "ghost".clone_into(&mut attempt.agent_id);
                broken.attempts.insert(attempt.attempt_id.clone(), attempt);
                broken.revision = self.acknowledged + 1;
                let error = self
                    .publish(&broken.tree(&self.templates, TREE, self.acknowledged))
                    .unwrap_err();
                assert_eq!(error.kind, TreeStoreErrorKind::Unavailable, "{error}");
            }
            Step::Reissue { slot, seed } => self.model.reissue(*slot, *seed),
            Step::Delete => {
                if self.acknowledged == 0 {
                    return;
                }
                self.store.delete(TREE, self.acknowledged).unwrap();
                self.reference.delete(TREE, self.acknowledged);
                self.acknowledged = 0;
            }
            Step::Reopen => {
                self.store = SqliteTreeStore::open(&self.rows_path()).unwrap();
            }
            Step::Peer { seed } => {
                if self.acknowledged == 0 {
                    return;
                }
                let mut peer_model = self.model.clone();
                peer_model.revision = self.acknowledged;
                peer_model.put(&self.templates, 5, (seed % 5) as u8, *seed);
                peer_model.remove(8, (seed % 5) as u8);
                let tree = peer_model.tree(&self.templates, TREE, self.acknowledged);
                let peer = SqliteTreeStore::open(&self.rows_path()).unwrap();
                let _ = peer.save(&tree).unwrap();
                self.reference.save(&tree).unwrap();
            }
        }
        assert_eq!(
            dump(&self.rows_path(), TREE),
            dump(&self.root.path().join("whole.sqlite3"), TREE),
            "rows diverged after {step:?}"
        );
    }

    /// The store reads back exactly the last acknowledged model.
    fn assert_loads_model(&self) {
        if self.acknowledged == 0 {
            return;
        }
        let loaded = self.store.load(TREE).unwrap().unwrap();
        let loaded = loaded.rows();
        let expected = self.model.tree(&self.templates, TREE, self.acknowledged);
        assert_eq!(loaded.revision, self.acknowledged);
        assert_eq!(loaded.grants(), expected.grants());
        assert_eq!(loaded.attempts(), expected.attempts());
        assert_eq!(loaded.candidates(), expected.candidates());
        assert_eq!(loaded.workspaces(), expected.workspaces());
        assert_eq!(loaded.messages(), expected.messages());
        assert_eq!(loaded.operations(), expected.operations());
        assert_eq!(loaded.commands(), expected.commands());
        assert_eq!(loaded.feed_cursors(), expected.feed_cursors());
        assert_eq!(loaded.nodes(), expected.nodes());
        assert_eq!(loaded.epochs(), expected.epochs());
        assert_eq!(loaded.forked_from(), expected.forked_from());
        assert_eq!(loaded.runner_ancestors(), expected.runner_ancestors());
        let tx = self.reference.0.unchecked_transaction().unwrap();
        let whole = rows::load_agent_tree(&tx, TREE).unwrap().unwrap();
        assert_eq!(whole.rows().attempts(), expected.attempts());
    }
}

proptest! {
    #![proptest_config(Config {
        cases: 64,
        max_shrink_iters: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(0x7265_6577),
        ..Config::default()
    })]

    #[test]
    fn row_level_saves_leave_the_rows_of_a_whole_tree_replace(
        steps in proptest::collection::vec(step(), 1..48),
    ) {
        let mut harness = Harness::new();
        for step in &steps {
            harness.run(step);
        }
        harness.run(&Step::Save { bump: true });
        harness.assert_loads_model();
    }
}

fn saved(harness: &mut Harness, bump: bool) -> ProductAgentTree {
    harness.run(&Step::Save { bump });
    harness
        .model
        .tree(&harness.templates, TREE, harness.acknowledged)
}

#[test]
fn a_stale_handle_is_refused_on_the_row_level_path_as_on_the_whole_tree_path() {
    let mut harness = Harness::new();
    harness.run(&Step::Put {
        table: 5,
        slot: 0,
        seed: 1,
    });
    // The first save has no image; the second compares and swaps.
    let first = saved(&mut harness, true);
    harness.run(&Step::Put {
        table: 5,
        slot: 1,
        seed: 2,
    });
    let current = saved(&mut harness, true);
    let before = dump(&harness.rows_path(), TREE);

    // Two handles loaded at the first revision; the other one published.
    let mut behind = current.clone();
    behind.stored_revision = first.revision;
    behind.revision = current.revision + 1;
    assert_eq!(
        harness.store.save(&behind).unwrap_err(),
        TreeStoreError::stale(current.revision, behind.revision)
    );
    // A handle that claims a revision the store never acknowledged.
    let mut ahead = current.clone();
    ahead.stored_revision = current.revision + 5;
    ahead.revision = current.revision + 6;
    assert_eq!(
        harness.store.save(&ahead).unwrap_err(),
        TreeStoreError::stale(current.revision, ahead.revision)
    );
    // A handle for a tree it believes was never published.
    let mut fresh = current.clone();
    fresh.stored_revision = 0;
    assert_eq!(
        harness.store.save(&fresh).unwrap_err(),
        TreeStoreError::stale(current.revision, fresh.revision)
    );
    assert_eq!(dump(&harness.rows_path(), TREE), before);

    // The acknowledged handle still publishes, and to the same rows.
    harness.run(&Step::Put {
        table: 6,
        slot: 0,
        seed: 3,
    });
    let _ = saved(&mut harness, true);
    harness.assert_loads_model();
}

#[test]
fn a_same_revision_write_by_another_connection_is_overwritten_whole() {
    let mut harness = Harness::new();
    for slot in 0..3 {
        harness.run(&Step::Put {
            table: 5,
            slot,
            seed: u32::from(slot),
        });
    }
    let _ = saved(&mut harness, true);
    let _ = saved(&mut harness, true);
    // Another connection publishes the same revision with other mail. This
    // store's image no longer describes the file, so its next save must not
    // diff against it.
    harness.run(&Step::Peer { seed: 7 });
    harness.run(&Step::Put {
        table: 8,
        slot: 0,
        seed: 9,
    });
    let _ = saved(&mut harness, false);
    harness.assert_loads_model();
}

#[test]
fn a_refused_save_leaves_rows_and_the_next_save_is_exact() {
    let mut harness = Harness::new();
    harness.run(&Step::Put {
        table: 2,
        slot: 0,
        seed: 4,
    });
    let _ = saved(&mut harness, true);
    let _ = saved(&mut harness, true);
    harness.run(&Step::Broken);
    harness.run(&Step::Remove { table: 2, slot: 0 });
    harness.run(&Step::Put {
        table: 0,
        slot: 1,
        seed: 5,
    });
    let _ = saved(&mut harness, true);
    harness.assert_loads_model();
}

#[test]
fn a_request_key_moves_to_a_new_attempt_in_one_save() {
    let mut harness = Harness::new();
    // Seed 3 makes a keyed attempt.
    harness.run(&Step::Put {
        table: 2,
        slot: 1,
        seed: 3,
    });
    let _ = saved(&mut harness, true);
    let _ = saved(&mut harness, true);
    harness.run(&Step::Reissue { slot: 1, seed: 4 });
    let tree = saved(&mut harness, true);
    let holders = tree
        .attempts()
        .values()
        .filter(|attempt| attempt.request_key.as_deref() == Some("key-1"))
        .map(|attempt| attempt.attempt_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(holders, ["b1"]);
    harness.assert_loads_model();
}

#[test]
fn a_deleted_tree_is_recreated_from_revision_zero() {
    let mut harness = Harness::new();
    harness.run(&Step::Put {
        table: 5,
        slot: 0,
        seed: 1,
    });
    let _ = saved(&mut harness, true);
    let _ = saved(&mut harness, true);
    harness.run(&Step::Delete);
    assert!(harness.store.load(TREE).unwrap().is_none());
    harness.run(&Step::Put {
        table: 6,
        slot: 0,
        seed: 2,
    });
    let tree = saved(&mut harness, true);
    assert_eq!(tree.revision, 1);
    let _ = saved(&mut harness, true);
    harness.assert_loads_model();
}
