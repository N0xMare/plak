//! Destination-owned semantic compilation, before any drain or native-store write.
use super::{Capsule, Custody};
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};
use plakous::sessions::AgentSessions;
use plakous_recursive_subagents::tree::{
    CustodyHolding, CustodyHost, CustodyRecord, HostInstance, MemoryTreeStore, ProductAgentTree,
    TreeStore,
};
use serde::Serialize;
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

const MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GroupProbe {
    group: String,
    trees: Vec<String>,
    sessions: Vec<String>,
    capacities: Vec<HeadProbe>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct HeadProbe {
    source: String,
    target: String,
    capacity: crate::carry::CarryCapacityProbe,
}

#[derive(serde::Deserialize)]
struct Header<'a> {
    kind: Option<&'a str>,
}

pub(crate) fn read_if_move(path: &Path) -> CliResult<Option<Capsule>> {
    let file = std::fs::File::open(path).map_err(|e| CliFailure::new(e.to_string()))?;
    if file
        .metadata()
        .map_err(|e| CliFailure::new(e.to_string()))?
        .len()
        > MAX_BYTES
    {
        return Err(CliFailure::new(
            "ownership-move document exceeds its byte bound",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| CliFailure::new(e.to_string()))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(CliFailure::new(
            "ownership-move document exceeds its byte bound",
        ));
    }
    let header: Header<'_> = serde_json::from_slice(&bytes)
        .map_err(|_| CliFailure::new("invalid session export header"))?;
    if header.kind != Some("host_upgrade") {
        return Ok(None);
    }
    let document: Capsule = serde_json::from_slice(&bytes)
        .map_err(|_| CliFailure::new("invalid ownership-move document"))?;
    document.validate().map_err(CliFailure::new)?;
    Ok(Some(document))
}

pub(crate) fn probe(services: &Services, capsule: Capsule) -> CliResult<GroupProbe> {
    capsule.validate().map_err(CliFailure::new)?;
    let tree_store = Arc::new(MemoryTreeStore::new());
    let operation = plakous::model::hex_lower(&capsule.operation);
    for tree in &capsule.trees {
        tree_store
            .install_move_seed(
                &operation,
                &tree.cut.digest().map_err(CliFailure::new)?,
                &tree.cut,
                &tree.custody.decode()?,
            )
            .map_err(|e| CliFailure::new(e.detail))?;
    }
    let products = capsule
        .products
        .into_iter()
        .map(|product| {
            let record = product.into_cut().map_err(CliFailure::new)?.into_parts().0;
            Ok((record.id.clone(), record))
        })
        .collect::<CliResult<std::collections::BTreeMap<_, _>>>()?;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliFailure::new(e.to_string()))?;
    let mut capacities = Vec::new();
    let mut rebuilt_sessions = Vec::new();
    let mut native_sessions = Vec::new();
    let operation = plakous::sessions::host_move::MoveOperation::new(capsule.operation)
        .map_err(|e| CliFailure::new(e.to_string()))?;
    for session in capsule.sessions {
        let header = session.header().map_err(CliFailure::new)?;
        let mut branches = Vec::new();
        let mut checkpoints = Vec::new();
        native_sessions.push(session.id);
        let compiler = AgentSessions::move_preflight_compiler(session.durability.decode())
            .map_err(|e| CliFailure::new(e.to_string()))?;
        for head in session.heads {
            let record = products
                .get(&head.context.product)
                .ok_or_else(|| CliFailure::new("move head has no source product configuration"))?
                .clone();
            let (capacity, rebuilt) = probe_head(
                services,
                &executor,
                &compiler,
                tree_store.clone(),
                head,
                record,
                operation,
            )?;
            capacities.push(capacity);
            branches.push(rebuilt.branch);
            checkpoints.push(rebuilt.checkpoint);
        }
        rebuilt_sessions.push(plakous::sessions::host_move::MoveSession {
            session: header,
            feed_tail_floor: session.feed_floor,
            branches,
            checkpoints,
        });
    }
    let lines = capsule
        .lines
        .into_iter()
        .map(super::Line::decode)
        .collect::<Result<_, _>>()
        .map_err(CliFailure::new)?;
    let install = plakous::sessions::host_move::MoveInstall::new(rebuilt_sessions, lines)
        .map_err(|e| CliFailure::new(e.to_string()))?;
    plakous::sessions::SessionBackend::preflight_move_install_sqlite(
        services.paths()?.runtime_database(),
        &install,
    )
    .map_err(|e| CliFailure::new(e.to_string()))?;
    Ok(GroupProbe {
        group: capsule.group,
        trees: capsule
            .trees
            .iter()
            .map(|tree| tree.cut.tree_id().into())
            .collect(),
        sessions: native_sessions,
        capacities,
    })
}

fn probe_head(
    services: &Services,
    executor: &tokio::runtime::Runtime,
    compiler: &AgentSessions,
    tree_store: Arc<MemoryTreeStore>,
    head: super::Head,
    mut record: plakous_hosted::sessions::SessionRecord,
    operation: plakous::sessions::host_move::MoveOperation,
) -> CliResult<(
    HeadProbe,
    plakous::sessions::host_move::ReconstructedMoveHead,
)> {
    head.context
        .configuration
        .clone()
        .apply(&mut record)
        .map_err(CliFailure::new)?;
    record.workspace_root.clone_from(&head.context.workspace);
    record.model = Some(head.binding.model.clone());
    record.node =
        plakous_hosted::sessions::SessionNode::parse(&head.context.tree, &head.context.agent)
            .map_err(CliFailure::new)?;
    record.plakous_locator = None;
    record.source_product_session_id = None;
    record.source_workflow_operation_id = None;
    let placement = crate::carry::upgrade::remote::Placement::prepare(
        services,
        &head.context.runner,
        &head.context.workspace,
    )?;
    let mut config = crate::commands::session::move_host_config(&placement.services, record)?;
    config.session.plakous_locator = None;
    let presentation = Arc::new(
        head.presentation
            .clone()
            .decode()
            .map_err(CliFailure::new)?,
    );
    config.state_roots = config
        .state_roots
        .with_move_compilation_data(presentation, tree_store.clone());
    let tree = tree_store
        .load(&head.context.tree)
        .map_err(|e| CliFailure::new(e.detail))?
        .map(ProductAgentTree::reopen);
    let estimated_tokens = head.history.estimated_tokens();
    let messages = head.history.message_count();
    let source = head.source.clone();
    let plan_authority = head.context.plan_authority.clone();
    let (capacity, reconstructed) = executor.block_on(async {
        let runtime = config
            .runtime_factory
            .move_runtime(&config, tree.as_ref(), &plan_authority, &mut Vec::new())
            .await?;
        let window = runtime.input_window();
        let capacity = crate::carry::CarryCapacityProbe {
            route: window.route,
            total_window: window.total_window,
            admitted_input: window.admitted_input,
            estimated_tokens,
            messages,
        };
        crate::carry::upgrade::require_capacity(&capacity)?;
        let request = head.into_request(operation).map_err(CliFailure::new)?;
        let rebuilt = compiler
            .reconstruct_move_head(runtime, request)
            .await
            .map_err(|e| CliFailure::new(e.to_string()))?;
        Ok::<_, CliFailure>((capacity, rebuilt))
    })?;
    let target = plakous::sessions::SessionLocator::new(
        reconstructed.branch.session.session_id(),
        reconstructed.branch.session.branch_id(),
        reconstructed.checkpoint.id(),
    );
    Ok((
        HeadProbe {
            source,
            target: target.to_string(),
            capacity,
        },
        reconstructed,
    ))
}

impl Custody {
    pub(super) fn decode(&self) -> CliResult<CustodyRecord> {
        if self.host.is_empty()
            || self.machine.is_empty()
            || self.fence == 0
            || self.renewed_ms < self.claimed_ms
        {
            return Err(CliFailure::new("invalid move tree custody"));
        }
        Ok(CustodyRecord {
            fence: self.fence,
            holding: CustodyHolding {
                host: CustodyHost {
                    host_id: self.host.clone(),
                    instance: HostInstance::parse(&self.instance).map_err(CliFailure::new)?,
                    binary: self.binary.clone(),
                    host_name: self.machine.clone(),
                },
                claimed_at_ms: self.claimed_ms,
                renewed_at_ms: self.renewed_ms,
            },
            released: true,
        })
    }
}
