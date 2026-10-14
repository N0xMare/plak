//! Bounded-memory half of the recursive-children resource test.
//!
//! The structural budgets live in `tests/topology_resources.rs`. The projected
//! snapshot cannot: `topology_navigator::project_snapshot` is `pub(crate)`, so
//! this file is included from `lib.rs` behind `#[cfg(test)]` rather than
//! written as a second integration test.
//!
//! What it asserts: a deep, wide tree grown until every spawn budget refuses
//! projects a snapshot whose heap footprint is bounded by the appendix, not by
//! how long the session ran. §11 also names bounded file descriptors and a
//! bounded snapshot rate; the projection is a pure in-memory read of an
//! already-built tree and opens nothing, so this file covers memory only.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test module may unwrap"
)]

use std::path::PathBuf;

use plakous_host_protocol::views::{NodeView, TreeView};

use plakous_hosted::agent_tree::ProductAgentTree;
use plakous_hosted::agent_tree_protocol::{
    MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES, MAX_DIRECT_CHILDREN, MAX_TREE_DEPTH, MAX_TREE_NODES,
    SpawnAgentRequest, SpawnWorkspace, TemplateName, Tier, WriterMode,
};
use plakous_hosted::orchestration::{OrchestrationSnapshot, RosterBinding};
use plakous_hosted::topology_navigator::project_snapshot;

const SCHEMA: &str = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}"#;

/// Ceiling this gate holds one projected snapshot to.
///
/// Derived, not guessed: at most `MAX_TREE_NODES` rows, each carrying a role
/// and a prompt the tree already bounds at `MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES`, plus a
/// generous allowance for the short label strings on every row.
const PER_ROW_LABEL_ALLOWANCE: usize = 4 * 1024;

fn snapshot_ceiling_bytes() -> usize {
    MAX_TREE_NODES * (2 * MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES + PER_ROW_LABEL_ALLOWANCE)
}

fn request(template: &str, description: &str) -> SpawnAgentRequest {
    SpawnAgentRequest {
        template: TemplateName::parse(template).expect("template name"),
        description: description.to_owned(),
        prompt: format!("bounded prompt for {description}"),
        output_schema_json: SCHEMA.to_owned(),
        tier: Tier::ReadOnly,
        workspace: SpawnWorkspace::Inherit,
        inputs: None,
        request_key: None,
    }
}

/// A working set of one leaf and one spawn-capable supervisor, both
/// read-only with an inherited workspace, so no git repository is involved.
fn recursive_epoch() -> OrchestrationSnapshot {
    let mut epoch = OrchestrationSnapshot::unpinned("selected-tuple");
    epoch.bind_hints = plakous_hosted::orchestration::BindHints::from_names(&["test-account"], &[]);
    let mut scout = RosterBinding::leaf("test-account", "selected-tuple");
    scout.max_tier = Tier::ReadOnly;
    epoch
        .roster
        .insert(TemplateName::parse("scout").expect("name"), scout);
    epoch
        .working_set
        .insert(TemplateName::parse("scout").expect("name"));
    let mut supervisor = RosterBinding::leaf("test-account", "selected-tuple");
    supervisor.max_tier = Tier::ReadOnly;
    supervisor.spawns = vec![TemplateName::parse("scout").expect("name")];
    epoch
        .roster
        .insert(TemplateName::parse("supervisor").expect("name"), supervisor);
    epoch
        .working_set
        .insert(TemplateName::parse("supervisor").expect("name"));
    epoch.max_depth = MAX_TREE_DEPTH;
    epoch.digest = epoch.compute_digest();
    epoch
}

/// Heap bytes one projected row owns, counting every `String` it holds.
fn node_bytes(node: &NodeView) -> usize {
    // The lifecycle fields are enums now, so they own no heap at all; only the
    // ids, briefs, and paths the tree already holds do.
    size_of::<NodeView>()
        + node.agent_id.len()
        + node.parent_agent_id.as_ref().map_or(0, String::len)
        + node.attempts.len() * size_of::<u32>()
        + node.candidate_id.as_ref().map_or(0, |id| id.as_str().len())
        + node.description.len()
        + node.prompt.len()
        + node.workspace_path.len()
        + node.template.as_ref().map_or(0, String::len)
        + node.model_account.len()
        + node.model_tuple.len()
        + node.model_effort.as_ref().map_or(0, String::len)
}

fn snapshot_bytes(snapshot: &TreeView) -> usize {
    size_of::<TreeView>()
        + snapshot.tree_id.len()
        + snapshot.nodes.iter().map(node_bytes).sum::<usize>()
}

/// A deep, wide tree grown until every spawn budget refuses.
fn saturated_tree(root: &std::path::Path) -> ProductAgentTree {
    let mut tree = ProductAgentTree::bootstrap_root(
        "tree1",
        "root",
        "owner",
        PathBuf::from(root),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("bootstrap");
    tree.install_snapshot(recursive_epoch());

    let mut caller = "root".to_owned();
    for _ in 0..MAX_TREE_DEPTH {
        // Fan out at this level, then step one level deeper. Both loops stop
        // on the first refusal; the budgets, not the loop, own the shape.
        for _ in 0..MAX_DIRECT_CHILDREN {
            if tree
                .spawn_agent(&caller, &request("scout", "scout"), None)
                .is_err()
            {
                break;
            }
        }
        let Ok(next) = tree.spawn_agent(&caller, &request("supervisor", "supervisor"), None) else {
            break;
        };
        caller = next.agent_id;
    }
    tree
}

#[test]
fn a_saturated_recursive_tree_projects_a_bounded_snapshot() {
    let root = tempfile::tempdir().expect("tempdir");
    let tree = saturated_tree(root.path());

    let snapshot = project_snapshot(&tree);
    assert!(
        !snapshot.nodes.is_empty(),
        "the projection must carry the tree it was given"
    );
    assert!(
        snapshot.nodes.len() <= MAX_TREE_NODES,
        "projected {} rows, above the {MAX_TREE_NODES} node ceiling",
        snapshot.nodes.len()
    );
    for node in &snapshot.nodes {
        assert!(
            node.depth <= MAX_TREE_DEPTH,
            "row {} projected depth {}, above the {MAX_TREE_DEPTH} ceiling",
            node.agent_id,
            node.depth
        );
        assert!(node.description.len() <= MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES);
        assert!(node.prompt.len() <= MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES);
    }

    let held = snapshot_bytes(&snapshot);
    let ceiling = snapshot_ceiling_bytes();
    assert!(
        held <= ceiling,
        "projected snapshot holds {held} bytes, above the {ceiling} byte ceiling"
    );
}

#[test]
fn projecting_the_same_tree_twice_allocates_the_same_bounded_snapshot() {
    // A snapshot that grew between two reads of an unchanged tree would mean
    // the projection accumulates rather than replaces (§11: "Replace, never
    // patch locally").
    let root = tempfile::tempdir().expect("tempdir");
    let tree = saturated_tree(root.path());

    let first = project_snapshot(&tree);
    let second = project_snapshot(&tree);
    assert_eq!(first, second, "projection is not a pure read of the tree");
    assert_eq!(snapshot_bytes(&first), snapshot_bytes(&second));
}
