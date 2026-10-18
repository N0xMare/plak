//! Catalogs, schemas and granted invocation.

use super::*;
use crate::tools::registrations::schema;

#[test]
fn topology_kinds_match_presentation_order() {
    assert_eq!(
        presentation_kinds().map(TopologyKind::name).as_slice(),
        TOPOLOGY_PRESENTATION_ORDER
    );
}

#[test]
fn root_catalog_omits_submit_result() {
    let root = tempfile::tempdir().unwrap();
    let tree = bootstrap_tree("tree1", root.path());
    let names = granted_names(Arc::new(Mutex::new(tree)), "root");
    assert!(names.iter().any(|name| name == "spawn_agent"));
    assert!(names.iter().any(|name| name == "adopt_candidate"));
    assert!(!names.iter().any(|name| name == "merge_workspace"));
    assert!(!names.iter().any(|name| name == "submit_result"));
}

#[test]
fn spawn_template_enum_lists_only_the_spawnable_working_set() {
    let spawnable = vec!["review".to_owned(), "docs".to_owned()];
    let spawn_schema = schema(TopologyKind::Spawn, &spawnable);
    let template_enum = spawn_schema["properties"]["template"]["enum"]
        .as_array()
        .unwrap();
    assert_eq!(
        template_enum
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["review", "docs"]
    );
    assert_eq!(
        spawn_schema["required"],
        json!(["template", "prompt", "output_schema_json", "tier"])
    );
    assert_eq!(
        spawn_schema["properties"]["tier"]["enum"],
        json!(["read_only", "execute", "mutate"])
    );
    for removed in [
        "class",
        "workspace_mode",
        "workspace_from",
        "workspace_id",
        "write_effect",
    ] {
        assert!(
            spawn_schema["properties"].get(removed).is_none(),
            "{removed}"
        );
    }
    let unpinned_schema = schema(TopologyKind::Spawn, &[]);
    assert_eq!(
        unpinned_schema["properties"]["template"]["enum"]
            .as_array()
            .unwrap()
            .as_slice(),
        [] as [Value; 0]
    );
}

/// The menu excludes the caller's own template, so a spawned
/// supervisor's `spawn_agent` cards differ from the root's.
#[test]
fn spawn_cards_exclude_the_callers_own_template() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("tree1", root.path());
    tree.grants_mut_for_test("root").max_depth = 2;
    let spawned = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    let shared = Arc::new(Mutex::new(tree));
    let root_registry = registry_for(Arc::clone(&shared), "root");
    let child_registry = registry_for(shared, spawned.agent_id);
    let name = ToolName::new("spawn_agent").unwrap();
    for (registry, expect_orchestrate) in [(root_registry, true), (child_registry, false)] {
        let description = registry.definition(&name).unwrap().description();
        assert_eq!(
            description.contains("- orchestrate:"),
            expect_orchestrate,
            "{description}"
        );
        assert!(description.contains("- review:"));
    }
}

#[test]
fn spawn_tool_description_includes_template_cards_not_accounts() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("tree1", root.path());
    pin_review(&mut tree, "grok", "grok-4", None);
    let registry = registry_for(Arc::new(Mutex::new(tree)), "root");
    let name = ToolName::new("spawn_agent").unwrap();
    let description = registry.definition(&name).unwrap().description();
    assert!(description.contains("Spawn a child from a template"));
    // The child starts at spawn; the card must not send the root to mail.
    assert!(description.contains("Do not call send_agent_message to start it"));
    assert!(description.contains("output_json"));
    assert!(description.contains("- review:"));
    assert!(description.contains("tier ≤ read_only"));
    assert!(!description.contains("grok"));
    assert!(!description.contains("account:"));
    assert!(!description.contains("merge_workspace"));
}

/// `tier` and the `output_schema_json` example are each stated once: in the
/// tool description, which every composition renders, and not again in the
/// property descriptions beside it.
#[test]
fn spawn_documents_tier_and_output_schema_exactly_once() {
    let spawn_schema = schema(TopologyKind::Spawn, &[]);
    let description = PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE;
    let output = spawn_schema["properties"]["output_schema_json"]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("Required fields: template, prompt, output_schema_json, tier"));
    assert!(description.contains("children share your checkout"));
    assert!(description.contains("workspace=fork"));
    assert!(description.contains("clipped to the template ceiling"));
    assert_eq!(
        spawn_schema["properties"]["workspace"]["enum"],
        json!(["inherit", "fork"])
    );
    assert!(
        spawn_schema["properties"]["inputs"]["description"]
            .as_str()
            .unwrap()
            .contains("workspace=fork")
    );
    assert!(!description.contains("mint_unavailable"));
    assert!(
        spawn_schema["properties"]["tier"]
            .get("description")
            .is_none()
    );
    // The advice must quote a shape the admission actually accepts. The
    // keywords admission fills in are stated by the refusal, not here
    // (luna emitted the example shape and was refused).
    assert!(description.contains(crate::tree::OUTPUT_SCHEMA_EXAMPLE));
    assert!(!output.contains(crate::tree::OUTPUT_SCHEMA_EXAMPLE));
    assert!(output.contains("no $ref and no combinators"));
    assert!(!output.contains("filled in when omitted"));
    assert!(!output.contains("PortableToolSchemaV2"));
    let mut example: Value = serde_json::from_str(crate::tree::OUTPUT_SCHEMA_EXAMPLE).unwrap();
    example["$schema"] = crate::tree::OUTPUT_SCHEMA_DIALECT.into();
    example["additionalProperties"] = false.into();
    assert!(PortableToolSchemaV2::new(example).is_ok());
}

#[test]
fn child_catalog_includes_submit_and_omits_spawn() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("tree1", root.path());
    let spawned = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    let names = granted_names(Arc::new(Mutex::new(tree)), spawned.agent_id);
    assert!(names.iter().any(|name| name == "submit_result"));
    assert!(names.iter().any(|name| name == "send_agent_message"));
    assert!(!names.iter().any(|name| name == "spawn_agent"));
    assert!(!names.iter().any(|name| name == "adopt_candidate"));
}

#[test]
fn spawn_template_binding_resolves_account_and_tuple() {
    let root = tempfile::tempdir().unwrap();
    let (_store, mut tree) = persist_fixture(&root);
    tree.set_node_account_for_test("root", "codex");
    pin_review(&mut tree, "grok", "grok-4", Some("high"));
    let spawned = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let node = tree.node(&spawned.agent_id).unwrap();
    assert_eq!(node.account, "grok");
    assert_eq!(node.tuple_id, "grok-4");
    assert_eq!(node.effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn invoke_granted_template_unknown_is_executor_failure() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.snapshot.roster.clear();
    pin_review(&mut tree, "grok", "grok-4", None);
    let driver = driver_on(tree, Arc::clone(&store));
    let explore = SpawnAgentRequest::test_named("explore", Tier::ReadOnly, "scout", "look");
    let outcome = invoke_granted_topology_tool_on(
        driver.clone(),
        "root".into(),
        "spawn_agent",
        &serde_json::to_value(explore).unwrap(),
        "op-unpin".into(),
        TopologyHostPorts::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&outcome)["code"], "template_unknown");
}

#[tokio::test]
async fn invoke_granted_omits_spawn_on_empty_roster() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.snapshot.roster.clear();
    tree.snapshot.digest = tree.snapshot.compute_digest();
    let err = invoke_granted_topology_tool_on(
        driver_on(tree, store),
        "root".into(),
        "spawn_agent",
        &serde_json::to_value(SpawnAgentRequest::test_review()).unwrap(),
        "op-omit".into(),
        TopologyHostPorts::default(),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("not granted"), "{}", err.message());
}

#[tokio::test]
async fn invoke_granted_list_agents_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let outcome = invoke_granted_topology_tool_on(
        driver_on(tree, store),
        "root".into(),
        "list_agents",
        &json!({}),
        "op-list".into(),
        TopologyHostPorts::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert!(outcome_json(&outcome)["agents"].is_array());
}

struct StubRecoverer {
    calls: AtomicUsize,
}

impl ChildRecoverer for StubRecoverer {
    fn recover(
        &self,
        agent_id: String,
        expected_attempt_id: Option<crate::protocol::AttemptId>,
    ) -> Pin<Box<dyn Future<Output = Result<RecoverAgentOutput, TopologyError>> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(RecoverAgentOutput {
                agent_id,
                attempt_id: expected_attempt_id,
                recovered: true,
                status: NodeStatus::Failed,
                attempt: Some(crate::protocol::AttemptStatus::Failed),
                failure_kind: Some(crate::tree::TURN_ABANDONED.to_owned()),
                workspace_recovery: None,
            })
        })
    }
}

/// `recover_agent` is authorized like `close_agent` before the host port
/// is asked, and a host with no port refuses with a typed reason.
#[tokio::test]
async fn invoke_granted_recover_agent_authorizes_then_asks_the_host() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    let child = tree
        .spawn_agent("root", &spawn_request(), None)
        .unwrap()
        .agent_id;
    tree.settle_for_test();
    let driver = driver_on(tree, store);
    let recoverer = Arc::new(StubRecoverer {
        calls: AtomicUsize::new(0),
    });
    let invoke = |caller: &str, target: &str, operation: &str, with_port: bool| {
        let driver = driver.clone();
        let caller = caller.to_owned();
        let arguments = json!({ "agent_id": target });
        let operation = operation.to_owned();
        let ports = TopologyHostPorts {
            recoverer: with_port.then(|| Arc::clone(&recoverer) as Arc<dyn ChildRecoverer>),
            ..TopologyHostPorts::default()
        };
        async move {
            invoke_granted_topology_tool_on(
                driver,
                caller,
                "recover_agent",
                &arguments,
                operation,
                ports,
            )
            .await
        }
    };

    let unported = invoke("root", &child, "op-recover-1", false).await.unwrap();
    assert_eq!(unported.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        outcome_json(&unported)["code"],
        "recovery_session_unavailable"
    );

    let denied = invoke("root", "root", "op-recover-2", true).await.unwrap();
    assert_eq!(denied.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&denied)["code"], "grant_denied");
    assert_eq!(recoverer.calls.load(Ordering::SeqCst), 0);

    let receipt = invoke("root", &child, "op-recover-3", true).await.unwrap();
    assert_eq!(receipt.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&receipt)["agent_id"], child.as_str());
    assert_eq!(outcome_json(&receipt)["recovered"], true);
    assert_eq!(recoverer.calls.load(Ordering::SeqCst), 1);

    let leaf = invoke(&child, &child, "op-recover-4", true).await;
    assert!(leaf.is_err(), "a leaf is not granted recover_agent");
}
