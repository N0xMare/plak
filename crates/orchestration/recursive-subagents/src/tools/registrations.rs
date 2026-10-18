//! Tool schemas and the registrations a node is granted.

use super::*;

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// The spawn enum is this node's menu (the working set for the root, its
/// own card's `spawns` list otherwise), never a fixed set.
pub(super) fn schema(kind: TopologyKind, spawnable_templates: &[String]) -> Value {
    match kind {
        TopologyKind::Spawn => object_schema(
            json!({
                "template": {
                    "type": "string",
                    "enum": spawnable_templates,
                    "description": "Template to spawn, from the cards below."
                },
                "description": {
                    "type": "string",
                    "description": "Short label for the attempt; does not select bind."
                },
                "prompt": {
                    "type": "string",
                    "description": "Task brief for the child agent."
                },
                "output_schema_json": {
                    "type": "string",
                    "description": "JSON string holding the child's submit_result object schema. Property types are object, array, string, number, integer, or boolean; no $ref and no combinators."
                },
                "tier": {
                    "type": "string",
                    "enum": ["read_only", "execute", "mutate"]
                },
                "workspace": {
                    "type": "string",
                    "enum": ["inherit", "fork"],
                    "description": "Omit or inherit to share your checkout. fork mints an isolated copy."
                },
                "inputs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 8,
                    "description": "One candidate id, from a finished child's wait or list row, to start this child's copy from instead of your current state. Requires workspace=fork: inherit has no copy of its own."
                },
                "request_key": {
                    "type": "string",
                    "description": "Your own key for this spawn. A repeat with the same key and the same brief returns the same child instead of spawning a second one; the same key on a different brief is refused request_key_conflict."
                }
            }),
            &["template", "prompt", "output_schema_json", "tier"],
        ),
        TopologyKind::Send => object_schema(
            json!({
                "agent_id": { "type": "string" },
                "message": { "type": "string" },
                "purpose": {
                    "type": "string",
                    "enum": ["coordinate", "finding", "question", "reply", "followup"],
                    "description": "Omit for coordinate. Use reply only with in_reply_to set to an existing message_id. Finished children are revivable until retired. Use followup to re-task one on a new attempt; a running child takes priority=urgent instead."
                },
                "priority": {
                    "type": "string",
                    "enum": ["deferred", "urgent"],
                    "description": "Omit for urgent from a parent to its running child; deferred otherwise."
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Omit on the first message to a child. Required only when purpose is reply."
                },
                "output_schema_json": {
                    "type": "string",
                    "description": "Optional, and only with purpose=followup: a new result schema for the new attempt. Omit to keep the schema the child already has."
                }
            }),
            &["agent_id", "message"],
        ),
        TopologyKind::List => object_schema(
            json!({
                "include_self": { "type": "boolean" },
                "include_completed": { "type": "boolean" },
                "include_read": { "type": "boolean" }
            }),
            &[],
        ),
        TopologyKind::Wait => object_schema(
            json!({
                "agent_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Child ids to watch; omit for the children that have not reported yet. Returns when any of them settles, with a row per watched id."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Omit for 300000. Capped at 3600000."
                }
            }),
            &[],
        ),
        TopologyKind::Interrupt | TopologyKind::Close | TopologyKind::Recover => object_schema(
            json!({
                "agent_id": { "type": "string" },
                "expected_attempt_id": {
                    "type": "string",
                    "description": "Refused with attempt_mismatch unless it is the child's current attempt."
                }
            }),
            &["agent_id"],
        ),
        TopologyKind::Submit => object_schema(
            json!({
                "turn_token": { "type": "integer", "minimum": 1 },
                "output_json": { "type": "string" }
            }),
            &["turn_token", "output_json"],
        ),
        TopologyKind::Adopt => object_schema(
            json!({
                "candidate_id": { "type": "string" },
                "expected_target": {
                    "type": "string",
                    "description": "Snapshot of your copy the adoption must land on; a moved copy is refused target_changed."
                },
                "evidence": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Review attempt ids recorded with the adoption."
                }
            }),
            &["candidate_id"],
        ),
        TopologyKind::Discard => object_schema(
            json!({ "workspace_id": { "type": "string" } }),
            &["workspace_id"],
        ),
        TopologyKind::TaskOutput => object_schema(
            json!({
                "task_ids": {
                    "description": "Process ids returned by run_terminal_command (N or pN) and child agent ids.",
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 128,
                    "items": { "type": "string" }
                },
                "timeout_ms": {
                    "description": "How long to await, in milliseconds, when this turn needs the result; clamped to 3600000 (1 hour). Omit or pass 0 for an immediate snapshot.",
                    "type": "integer",
                    "minimum": 0
                }
            }),
            &["task_ids"],
        ),
        TopologyKind::TaskKill => object_schema(
            json!({
                "task_id": {
                    "description": "Process id (N or pN) to stop, or child agent id to interrupt.",
                    "type": "string"
                }
            }),
            &["task_id"],
        ),
        TopologyKind::JobsWait => object_schema(
            json!({
                "ids": {
                    "description": "Process ids returned by bash (N or pN) and child agent ids; omit for every child that has not reported and every own process",
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 128,
                    "items": { "type": "string" }
                },
                "timeout_ms": {
                    "description": "Omit for 300000; clamped to 3600000; 0 returns a snapshot",
                    "type": "integer",
                    "minimum": 0
                }
            }),
            &[],
        ),
        TopologyKind::JobsStop => object_schema(
            json!({
                "id": {
                    "description": "Process id returned by bash (N or pN) to stop, or child agent id to interrupt",
                    "type": "string"
                }
            }),
            &["id"],
        ),
    }
}

/// The caller-specific parts of the `spawn_agent` description.
struct SpawnDescription {
    pub(super) nodes: usize,
    pub(super) children: usize,
    pub(super) cards: String,
}

#[allow(
    clippy::too_many_arguments,
    reason = "one registration binds every collaborator the executor owns"
)]
fn registration(
    driver: TreeDriver,
    caller_id: Arc<Mutex<String>>,
    kind: TopologyKind,
    spawn_description: Option<&SpawnDescription>,
    spawnable_templates: &[String],
    binder: Option<Arc<dyn SpawnedSessionBinder>>,
    deliverer: Option<Arc<dyn MailDeliverer>>,
    canceller: Option<Arc<dyn LiveTurnCanceller>>,
    recoverer: Option<Arc<dyn ChildRecoverer>>,
    observer: Option<Arc<dyn TopologyObserver>>,
    cell_budget: Arc<CellAgentBudget>,
    vocabulary: TopologyVocabulary,
) -> TreeRuntimeResult<ToolRegistration> {
    let name = kind.name();
    let description = match (kind, spawn_description) {
        (TopologyKind::Spawn, Some(spawn)) => {
            // The Code Mode `exec` catalog renders a nested tool as a bare
            // TypeScript declaration and drops per-field descriptions, so
            // the `output_schema_json` shape has to be stated here or a
            // Code Mode root never sees it.
            PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE
                .replace("{await}", vocabulary.presented_name("wait_agent"))
                .replace("{nodes}", &spawn.nodes.to_string())
                .replace("{children}", &spawn.children.to_string())
                .replace("{cards}", &spawn.cards)
        }
        (TopologyKind::TaskOutput, _) => PLAK_TOPOLOGY_TASK_OUTPUT_DESCRIPTION.to_owned(),
        (TopologyKind::TaskKill, _) => PLAK_TOPOLOGY_TASK_KILL_DESCRIPTION.to_owned(),
        (TopologyKind::JobsWait, _) => PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION.to_owned(),
        (TopologyKind::JobsStop, _) => PLAK_TOPOLOGY_JOBS_STOP_DESCRIPTION.to_owned(),
        (TopologyKind::Send, _) => PLAK_TOPOLOGY_SEND_DESCRIPTION.to_owned(),
        (TopologyKind::List, _) => PLAK_TOPOLOGY_LIST_DESCRIPTION.to_owned(),
        (TopologyKind::Wait, _) => PLAK_TOPOLOGY_WAIT_DESCRIPTION.to_owned(),
        (TopologyKind::Submit, _) => PLAK_TOPOLOGY_SUBMIT_DESCRIPTION.to_owned(),
        (TopologyKind::Interrupt, _) => PLAK_TOPOLOGY_INTERRUPT_DESCRIPTION.to_owned(),
        (TopologyKind::Close, _) => PLAK_TOPOLOGY_CLOSE_DESCRIPTION.to_owned(),
        (TopologyKind::Recover, _) => PLAK_TOPOLOGY_RECOVER_DESCRIPTION.to_owned(),
        (TopologyKind::Adopt, _) => PLAK_TOPOLOGY_ADOPT_DESCRIPTION_TEMPLATE
            .replace("{await}", vocabulary.presented_name("wait_agent")),
        (TopologyKind::Discard, _) => PLAK_TOPOLOGY_DISCARD_DESCRIPTION.to_owned(),
        _ => PLAK_TOPOLOGY_OPERATION_FALLBACK_TEMPLATE.replace("{name}", name),
    };
    let definition = ToolDefinition::new(
        ToolName::new(name).map_err(|_| TreeRuntimeError::new("topology tool name is invalid"))?,
        "plak.topology.v1",
        description,
        ToolInputContract::structured(schema(kind, spawnable_templates))
            .map_err(|_| TreeRuntimeError::new("topology tool schema is invalid"))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        if kind.mutating() {
            ToolEffect::Mutating
        } else {
            ToolEffect::ReadOnly
        },
    )
    .map_err(|_| TreeRuntimeError::new("topology tool definition is invalid"))?;
    let registration = ToolRegistration::new(
        RegisteredToolId::new(format!("plak.topology.{name}.v1"))
            .map_err(|_| TreeRuntimeError::new("topology tool id is invalid"))?,
        ToolExecutorId::new(format!("plak.topology.{name}.executor.v1"))
            .map_err(|_| TreeRuntimeError::new("topology executor id is invalid"))?,
        ToolExecutorBindingIdentity::new(1, &[ISSUER, name.as_bytes()])
            .map_err(|_| TreeRuntimeError::new("topology binding identity is invalid"))?,
        definition,
        Arc::new(TopologyTool {
            driver,
            caller_id,
            kind,
            vocabulary,
            binder,
            deliverer,
            canceller,
            recoverer,
            observer,
            cell_budget,
        }),
    )
    .nested_eligible();
    // Only these registrations (Grok's output and kill, pi's wait and stop)
    // may claim the process pack's core entries of the same names. Any other
    // same-name capability is a namespace clash.
    Ok(
        if matches!(
            kind,
            TopologyKind::TaskOutput
                | TopologyKind::TaskKill
                | TopologyKind::JobsWait
                | TopologyKind::JobsStop
        ) {
            registration.presented_name_claimer()
        } else {
            registration
        },
    )
}

/// Grant-compiled Tact names plus occupancy tools, nested-eligible, bound to
/// one tree. The host decides what an extension set is; the tree hands back
/// the registrations it grants and nothing more.
pub fn topology_tool_registrations(
    tree: Arc<Mutex<ProductAgentTree>>,
    caller_id: impl Into<String>,
) -> TreeRuntimeResult<Vec<ToolRegistration>> {
    topology_tool_registrations_in(tree, caller_id, TopologyVocabulary::default())
}

/// [`topology_tool_registrations`] with the await and interrupt slots
/// spelled in `vocabulary`.
pub fn topology_tool_registrations_in(
    tree: Arc<Mutex<ProductAgentTree>>,
    caller_id: impl Into<String>,
    vocabulary: TopologyVocabulary,
) -> TreeRuntimeResult<Vec<ToolRegistration>> {
    let driver = driver_for_tree(&tree)?;
    topology_tool_registrations_on(
        driver,
        caller_id,
        TopologyHostPorts {
            vocabulary,
            ..TopologyHostPorts::default()
        },
    )
}

/// The host's session-side ports a topology tool reaches. Each one is
/// optional: a tool whose port is absent refuses rather than pretending.
#[derive(Clone, Default)]
pub struct TopologyHostPorts {
    /// Exact session-owned process services for topology waits and settlement.
    pub job_host: Option<Arc<dyn plakous::tools::JobHost>>,
    /// Opens a spawned child's session and starts its assignment turn.
    pub binder: Option<Arc<dyn SpawnedSessionBinder>>,
    /// Delivers mail onto a bound child session.
    pub deliverer: Option<Arc<dyn MailDeliverer>>,
    /// Cancels live child turns after an interrupt or close.
    pub canceller: Option<Arc<dyn LiveTurnCanceller>>,
    /// Settles a `recovery_required` child by an exact session reopen.
    pub recoverer: Option<Arc<dyn ChildRecoverer>>,
    /// Telemetry for node lifecycle.
    pub observer: Option<Arc<dyn TopologyObserver>>,
    /// How the node's pack spells the await and interrupt slots.
    pub vocabulary: TopologyVocabulary,
}

/// The registrations over an explicit driver: the tree, its store, and its port.
pub fn topology_tool_registrations_on(
    driver: TreeDriver,
    caller_id: impl Into<String>,
    ports: TopologyHostPorts,
) -> TreeRuntimeResult<Vec<ToolRegistration>> {
    driver.bind_job_host(ports.job_host.clone());
    let TopologyHostPorts {
        job_host: _,
        binder,
        deliverer,
        canceller,
        recoverer,
        observer,
        vocabulary,
    } = ports;
    let caller_id = caller_id.into();
    let (compiled, spawn_description, spawnable_templates, cell_budget) = {
        let tree = driver
            .tree
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology tree lock is poisoned"))?;
        let cell_budget = tree.cell_budget();
        let compiled = tree
            .compiled_tool_names(&caller_id)
            .map_err(|error| TreeRuntimeError::new(error.message))?;
        let caller_template = tree
            .node(&caller_id)
            .map_err(|error| TreeRuntimeError::new(error.message))?
            .template
            .as_ref()
            .map(|template| template.as_str().to_owned());
        let spawn_description = if compiled
            .iter()
            .any(|name| *name == TopologyKind::Spawn.name())
        {
            let grant = tree
                .grant(&caller_id)
                .map_err(|error| TreeRuntimeError::new(error.message))?;
            Some(SpawnDescription {
                nodes: MAX_TREE_NODES.min(usize::from(grant.max_nodes)),
                children: MAX_DIRECT_CHILDREN.min(usize::from(grant.max_children)),
                cards: tree.snapshot.template_cards(caller_template.as_deref()),
            })
        } else {
            None
        };
        let spawnable_templates: Vec<String> = tree
            .snapshot
            .spawnable_template_names(caller_template.as_deref())
            .map(|name| name.as_str().to_owned())
            .collect();
        (
            compiled,
            spawn_description,
            spawnable_templates,
            cell_budget,
        )
    };
    let caller_id = Arc::new(Mutex::new(caller_id));
    let mut registrations = Vec::new();
    for slot in presentation_kinds() {
        if !compiled.iter().any(|name| *name == slot.name()) {
            continue;
        }
        registrations.push(registration(
            driver.clone(),
            Arc::clone(&caller_id),
            vocabulary.presents(slot),
            spawn_description.as_ref(),
            &spawnable_templates,
            binder.as_ref().map(Arc::clone),
            deliverer.as_ref().map(Arc::clone),
            canceller.as_ref().map(Arc::clone),
            recoverer.as_ref().map(Arc::clone),
            observer.as_ref().map(Arc::clone),
            Arc::clone(&cell_budget),
            vocabulary,
        )?);
    }
    Ok(registrations)
}

/// Idle host / JSONL `invoke_tool`: same TopologyTool executor as a model
/// call, over the hub's own driver (tree, store, and port).
///
/// Grant compile is not bypassed. Unknown coding names are rejected. Does not
/// admit a Plakous turn or write a fake tool call into the conversation prefix.
pub async fn invoke_granted_topology_tool_on(
    driver: TreeDriver,
    caller_id: String,
    name: &str,
    arguments: &Value,
    operation_id: String,
    ports: TopologyHostPorts,
) -> TreeRuntimeResult<ToolOutcome> {
    driver.bind_job_host(ports.job_host.clone());
    let TopologyHostPorts {
        job_host: _,
        binder,
        deliverer,
        canceller,
        recoverer,
        ..
    } = ports;
    let tree = Arc::clone(&driver.tree);
    let Some(kind) = TopologyKind::from_name(name) else {
        return Err(TreeRuntimeError::new(format!(
            "invoke_tool name must be a topology tool (`{name}` is not)"
        )));
    };
    let compiled = {
        let tree = tree
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology tree lock is poisoned"))?;
        tree.compiled_tool_names(&caller_id)
            .map_err(|error| TreeRuntimeError::new(error.message))?
    };
    if !compiled.contains(&name) {
        return Err(TreeRuntimeError::new(format!(
            "topology tool `{name}` is not granted to `{caller_id}`"
        )));
    }
    let cell_budget = {
        let tree = tree
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology tree lock is poisoned"))?;
        tree.cell_budget()
    };
    let tool = TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new(caller_id)),
        kind,
        vocabulary: TopologyVocabulary::default(),
        binder,
        deliverer,
        canceller,
        recoverer,
        observer: None,
        cell_budget,
    };
    // The idle host and JSONL `invoke_tool` are not a cell: `Direct` lineage
    // carries no cell identity, so the per-cell agent budget never applies.
    tool.dispatch(&ToolCallerLineage::Direct, operation_id, arguments)
        .await
        .map_err(|error| TreeRuntimeError::new(error.to_string()))
}
