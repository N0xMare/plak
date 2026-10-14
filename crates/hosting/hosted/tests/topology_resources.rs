//! Recursive-children resource test for the product agent tree.
//!
//! §11 asks for "a resource test with recursive children — a deep, wide tree
//! under live snapshot pressure — asserting bounded memory, bounded file
//! descriptors, and a bounded snapshot rate". This file covers the structural
//! half that is reachable from outside the crate: a depth-3 recursive spine
//! and the four spawn budgets that clamp it, driven entirely through the
//! public `ProductAgentTree` API.
//!
//! Every case is offline and deterministic. Every template here caps at
//! `read_only`, which binds the parent's checkout (`WorkspaceMode::Inherit`),
//! so no git repository and no provider session is involved; child ids come
//! from a deterministic counter and every map is a `BTreeMap`.
//!
//! The projected-snapshot memory bound lives in
//! `crates/hosting/hosted/src/topology_resources_projection.rs`: the projection
//! function `topology_navigator::project_snapshot` is `pub(crate)`, so it
//! cannot be reached from an integration test.

#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test crate may unwrap"
)]

use std::path::PathBuf;

use plakous_hosted::agent_tree::ProductAgentTree;
use plakous_hosted::agent_tree_protocol::{
    AttemptScopedRequest, MAX_DIRECT_CHILDREN, MAX_RUNNING_CHILDREN, MAX_TREE_DEPTH,
    SpawnAgentRequest, SpawnWorkspace, TemplateName, Tier, TopologyErrorCode, WriterMode,
};
use plakous_hosted::orchestration::{OrchestrationSnapshot, RosterBinding};

const SCHEMA: &str = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}"#;

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

/// A working set of four read-only templates on one test account:
/// `supervisor` and `deputy` may spawn every template (a node's menu excludes
/// its own template, so a recursive spine alternates them), `critic` and
/// `scout` are leaves.
fn working_set(tuple_id: &str) -> OrchestrationSnapshot {
    let mut snapshot = OrchestrationSnapshot::unpinned(tuple_id);
    snapshot.bind_hints =
        plakous_hosted::orchestration::BindHints::from_names(&["test-account"], &[]);
    let names = ["supervisor", "deputy", "critic", "scout"]
        .map(|name| TemplateName::parse(name).expect("template name"));
    for (name, may_spawn) in [
        ("supervisor", true),
        ("deputy", true),
        ("critic", false),
        ("scout", false),
    ] {
        let mut binding = RosterBinding::leaf("test-account", tuple_id);
        binding.max_tier = Tier::ReadOnly;
        name.clone_into(&mut binding.template);
        if may_spawn {
            binding.spawns = names.to_vec();
        }
        let name = TemplateName::parse(name).expect("template name");
        snapshot.working_set.insert(name.clone());
        snapshot.roster.insert(name, binding);
    }
    snapshot.digest = snapshot.compute_digest();
    snapshot
}

/// The supervisor template for depth `depth` (1-based): the spine alternates
/// `supervisor` and `deputy` because a node cannot spawn its own template.
fn supervisor_at(depth: u8) -> &'static str {
    if depth % 2 == 1 {
        "supervisor"
    } else {
        "deputy"
    }
}

/// A tree whose frozen epoch permits recursion to `max_depth`.
///
/// `install_snapshot` is the only public way to move the root grant's bounds,
/// and it clamps every field to the appendix ceiling on the way in.
fn recursive_tree(root: &std::path::Path, requested_depth: u8) -> ProductAgentTree {
    let mut tree = ProductAgentTree::bootstrap_root(
        "tree1",
        "root",
        "owner",
        PathBuf::from(root),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("bootstrap");
    let mut snapshot = working_set("selected-tuple");
    snapshot.max_depth = requested_depth;
    snapshot.digest = snapshot.compute_digest();
    tree.install_snapshot(snapshot);
    tree
}

#[test]
fn a_depth_three_supervisor_spine_spawns_and_then_stops_at_the_frozen_ceiling() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut tree = recursive_tree(root.path(), MAX_TREE_DEPTH);

    // Recurse: root -> d1 -> d2 -> d3 -> d4. Only the supervisors may spawn.
    let mut caller = "root".to_owned();
    let mut spine = Vec::new();
    for depth in 1..=MAX_TREE_DEPTH {
        let template = supervisor_at(depth);
        let output = tree
            .spawn_agent(&caller, &request(template, template), None)
            .unwrap_or_else(|error| panic!("spawn at depth {depth} refused: {error:?}"));
        let node = tree.node(&output.agent_id).expect("spawned node");
        assert_eq!(node.depth, depth, "depth must increase by exactly one");
        assert_eq!(node.parent_agent_id.as_deref(), Some(caller.as_str()));
        assert_eq!(output.tier, Tier::ReadOnly);
        caller = output.agent_id.clone();
        spine.push(output.agent_id);
    }
    assert_eq!(spine.len(), usize::from(MAX_TREE_DEPTH));

    // The deepest node's grant no longer carries `may_spawn`, so recursion
    // terminates rather than running away.
    let next = supervisor_at(MAX_TREE_DEPTH + 1);
    let refusal = tree
        .spawn_agent(&caller, &request(next, next), None)
        .expect_err("recursion past the frozen depth must be refused");
    assert_eq!(refusal.code, TopologyErrorCode::GrantDenied, "{refusal:?}");
}

#[test]
fn a_leaf_template_cannot_recurse_at_all() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut tree = recursive_tree(root.path(), MAX_TREE_DEPTH);

    let reviewer = tree
        .spawn_agent("root", &request("critic", "critic"), None)
        .expect("critic spawn");
    assert_eq!(tree.node(&reviewer.agent_id).expect("node").depth, 1);

    let refusal = tree
        .spawn_agent(&reviewer.agent_id, &request("scout", "scout"), None)
        .expect_err("a leaf may not spawn");
    assert_eq!(refusal.code, TopologyErrorCode::GrantDenied, "{refusal:?}");
}

/// a node's menu is its own card's spawns minus its own template; the
/// root's is the working set.
#[test]
fn a_supervisor_cannot_spawn_its_own_template_but_the_root_can() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut tree = recursive_tree(root.path(), MAX_TREE_DEPTH);
    let supervisor = tree
        .spawn_agent("root", &request("supervisor", "supervisor"), None)
        .expect("root spawns a supervisor");
    let refusal = tree
        .spawn_agent(
            &supervisor.agent_id,
            &request("supervisor", "supervisor"),
            None,
        )
        .expect_err("a supervisor's menu excludes itself");
    assert_eq!(
        refusal.code,
        TopologyErrorCode::TemplateNotSpawnable,
        "{refusal:?}"
    );
    tree.spawn_agent(&supervisor.agent_id, &request("scout", "scout"), None)
        .expect("the rest of the supervisor's card stays spawnable");
}

/// asking above `max_tier` clips; the receipt reports the tier
/// the child actually got.
#[test]
fn a_tier_above_the_template_ceiling_clips_and_the_receipt_says_so() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut tree = recursive_tree(root.path(), MAX_TREE_DEPTH);
    let mut ambitious = request("critic", "critic");
    ambitious.tier = Tier::Mutate;
    let receipt = tree
        .spawn_agent("root", &ambitious, None)
        .expect("clipped spawn");
    assert_eq!(receipt.tier, Tier::ReadOnly);
    assert_eq!(
        tree.node(&receipt.agent_id).expect("node").tier,
        Tier::ReadOnly
    );
}

#[test]
fn an_epoch_cannot_raise_the_bounds_above_the_appendix_ceilings() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut tree = ProductAgentTree::bootstrap_root(
        "tree1",
        "root",
        "owner",
        PathBuf::from(root.path()),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("bootstrap");

    // Ask for far more than the appendix permits. `install_snapshot` clamps.
    let mut snapshot = working_set("selected-tuple");
    snapshot.max_depth = 200;
    snapshot.max_children = 200;
    snapshot.max_nodes = 200;
    snapshot.digest = snapshot.compute_digest();
    tree.install_snapshot(snapshot);

    // The clamp is not observable as a field (grants are crate-private), so it
    // is asserted where it bites: the depth spine still stops at the ceiling.
    let mut caller = "root".to_owned();
    for depth in 1..=MAX_TREE_DEPTH {
        let template = supervisor_at(depth);
        caller = tree
            .spawn_agent(&caller, &request(template, template), None)
            .expect("spawn within the clamped depth")
            .agent_id;
    }
    let next = supervisor_at(MAX_TREE_DEPTH + 1);
    let refusal = tree
        .spawn_agent(&caller, &request(next, next), None)
        .expect_err("depth stays clamped to the appendix ceiling");
    assert_eq!(refusal.code, TopologyErrorCode::GrantDenied, "{refusal:?}");
}

#[test]
fn a_flat_fan_out_hits_the_child_budget_before_running_saturation() {
    // `MAX_RUNNING_CHILDREN` counts executing children only — the root's own
    // turn is free — so a single parent meets `MAX_DIRECT_CHILDREN` (8) first.
    // The refusal must arrive as a budget error, not as an unbounded tree.
    let root = tempfile::tempdir().expect("tempdir");
    let mut tree = recursive_tree(root.path(), MAX_TREE_DEPTH);

    let mut live = Vec::new();
    let mut refusal = None;
    for _ in 0..=MAX_DIRECT_CHILDREN {
        match tree.spawn_agent("root", &request("critic", "critic"), None) {
            Ok(output) => live.push(output.agent_id),
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }
    let error = refusal.expect("a wide fan-out must hit a budget");
    assert_eq!(error.code, TopologyErrorCode::SpawnBudget, "{error:?}");
    assert!(error.message.contains("direct child budget"), "{error:?}");
    assert_eq!(live.len(), MAX_DIRECT_CHILDREN);

    // Closing a child frees the budget again: the bound is on live nodes, not
    // on ids ever issued.
    let closed = tree
        .close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: live[0].clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .expect("root may close its own child");
    assert_eq!(closed.len(), 1);
    tree.spawn_agent("root", &request("critic", "critic"), None)
        .expect("a closed slot is reusable");
}

#[test]
fn a_nested_fan_out_saturates_the_running_children_budget() {
    // Eight executing children anywhere in the tree is `capacity`; the root
    // never consumes a slot, so the bound is only reachable when children
    // spawn children. The error names the running children so the caller can
    // pick what to interrupt or wait on. There is no queue.
    let root = tempfile::tempdir().expect("tempdir");
    let mut tree = recursive_tree(root.path(), MAX_TREE_DEPTH);

    let mut live = Vec::new();
    for _ in 0..2 {
        let supervisor = tree
            .spawn_agent("root", &request("supervisor", "supervisor"), None)
            .expect("supervisor spawn")
            .agent_id;
        live.push(supervisor.clone());
        for _ in 0..3 {
            let leaf = tree
                .spawn_agent(&supervisor, &request("critic", "critic"), None)
                .expect("leaf spawn")
                .agent_id;
            live.push(leaf);
        }
    }
    assert_eq!(live.len(), MAX_RUNNING_CHILDREN);

    let error = tree
        .spawn_agent("root", &request("scout", "scout"), None)
        .expect_err("the ninth running child must refuse");
    assert_eq!(error.code, TopologyErrorCode::Capacity, "{error:?}");
    assert!(error.message.contains("no queue"), "{error:?}");
    assert!(
        !error.message.contains("root"),
        "the root is not part of the running set: {error:?}"
    );
    for id in &live {
        assert!(error.message.contains(id.as_str()), "{error:?}");
    }

    // Closing a subtree releases its execution slots.
    tree.close_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: live[0].clone(),
            expected_attempt_id: None,
        },
        None,
    )
    .expect("root may close its own child");
    tree.spawn_agent("root", &request("scout", "scout"), None)
        .expect("closing a subtree frees the running budget");
}

#[test]
fn deep_and_wide_together_stay_inside_every_bound() {
    // The §11 shape: a deep spine with siblings hanging off it, grown until
    // every budget refuses, then audited. Nothing may exceed the appendix.
    let root = tempfile::tempdir().expect("tempdir");
    let mut tree = recursive_tree(root.path(), MAX_TREE_DEPTH);

    let mut issued = vec!["root".to_owned()];
    let mut caller = "root".to_owned();
    let mut refusals = 0_usize;

    for depth in 1..=MAX_TREE_DEPTH {
        // One recursive step down, then fan out until the budget refuses.
        let template = supervisor_at(depth);
        let Ok(next) = tree.spawn_agent(&caller, &request(template, template), None) else {
            refusals += 1;
            break;
        };
        issued.push(next.agent_id.clone());
        for _ in 0..MAX_DIRECT_CHILDREN {
            match tree.spawn_agent(&caller, &request("scout", "scout"), None) {
                Ok(child) => issued.push(child.agent_id),
                Err(error) => {
                    assert!(
                        matches!(
                            error.code,
                            TopologyErrorCode::SpawnBudget | TopologyErrorCode::Capacity
                        ),
                        "{error:?}"
                    );
                    refusals += 1;
                    break;
                }
            }
        }
        caller = next.agent_id;
    }

    assert!(refusals > 0, "the tree grew without ever refusing");
    let mut max_depth = 0_u8;
    for id in &issued {
        let node = tree.node(id).expect("issued node is retrievable");
        max_depth = max_depth.max(node.depth);
    }
    assert!(
        max_depth <= MAX_TREE_DEPTH,
        "depth {max_depth} exceeded the appendix ceiling {MAX_TREE_DEPTH}"
    );
    assert!(
        issued.len() <= MAX_RUNNING_CHILDREN * usize::from(MAX_TREE_DEPTH) + 1,
        "issued {} nodes, far above what the running budget can hold live",
        issued.len()
    );
}
