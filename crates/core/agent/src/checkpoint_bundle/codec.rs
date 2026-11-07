//! The persisted codec: bundling, advancing, inspecting and materializing.

#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use plakous_model::{CanonicalHistory, CheckpointId, Digest, WorkingProjection};

use crate::RuntimeCheckpoint;
use crate::manifest::{
    DurableOperationRecord, OperationRecordWire, ReconstructiveComponent, RuntimeCanonicalState,
    RuntimeContextState, RuntimeManifestError, RuntimeOperationLedger, assemble_bundle_manifest,
    decode_bundle_metadata,
};

use super::index::cas_insert_all;
use super::object::{
    canonical_binding, canonical_command, canonical_component, canonical_family_artifact,
    canonical_operation_wire, canonical_provider_configuration, canonical_stable_prefix,
    canonical_tools, component_object_address,
};
use super::operation_tree::materialize_operation_tree;
use super::wire::{
    parse_canonical, parse_checkpoint_root, parse_ledger, parse_manifest_root,
    parse_operation_node, parse_sequence_node, parse_working,
};
use super::{
    BundleBuilder, CheckpointBundleError, CheckpointBundleStats, CheckpointObjectBundle,
    MAX_CHECKPOINT_OBJECT_BYTES, MAX_CHECKPOINT_OBJECTS, ObjectIndex, ObjectLayer,
    RuntimeCheckpointCodec, RuntimeCheckpointInspection, RuntimeObject, RuntimeObjectAddress,
    RuntimeObjectKind,
};

impl RuntimeCheckpointCodec {
    /// Reads diagnostic metadata and only the ledger paths named by Working obligations.
    /// The loader must reconstruct each object with `RuntimeObject::from_store`.
    /// This is not a full graph validation or permission to resume a runtime.
    ///
    /// # Errors
    /// Refuses invalid root identities, malformed metadata and missing obligation requests.
    pub fn inspect_metadata<E>(
        checkpoint_id: CheckpointId,
        integrity: Digest,
        root: RuntimeObjectAddress,
        mut load: impl FnMut(RuntimeObjectAddress) -> Result<RuntimeObject, E>,
        error: impl Fn(CheckpointBundleError) -> E,
    ) -> Result<RuntimeCheckpointInspection, E> {
        let mut inspected = BTreeMap::<RuntimeObjectAddress, RuntimeObject>::new();
        let mut retained_bytes = 0_usize;
        let mut read = |address: RuntimeObjectAddress, kind| {
            if address.kind() != kind {
                return Err(error(CheckpointBundleError::WrongKind));
            }
            if let Some(object) = inspected.get(&address) {
                return Ok(object.clone());
            }
            if inspected.len() == MAX_CHECKPOINT_OBJECTS {
                return Err(error(CheckpointBundleError::BoundExceeded));
            }
            let object = load(address)?;
            retained_bytes = retained_bytes.saturating_add(object.retained_bytes());
            if retained_bytes > MAX_CHECKPOINT_OBJECT_BYTES {
                return Err(error(CheckpointBundleError::BoundExceeded));
            }
            if object.address() != address {
                return Err(error(CheckpointBundleError::InvalidAddress));
            }
            inspected.insert(address, object.clone());
            Ok(object)
        };
        let object = read(root, RuntimeObjectKind::CheckpointRoot)?;
        let (stored_id, stored_integrity, manifest) =
            parse_checkpoint_root(object.canonical_bytes()).map_err(&error)?;
        if stored_id != checkpoint_id || stored_integrity != integrity {
            return Err(error(CheckpointBundleError::SemanticDisagreement));
        }
        let object = read(manifest, RuntimeObjectKind::ManifestRoot)?;
        let manifest = parse_manifest_root(object.canonical_bytes()).map_err(&error)?;
        let object = read(manifest.metadata, RuntimeObjectKind::ManifestMetadata)?;
        let metadata =
            decode_bundle_metadata(object.canonical_bytes()).map_err(|e| error(e.into()))?;
        let mut requests = Vec::new();
        if let crate::manifest::RuntimeLifecycle::Working { obligations, .. } = &metadata.lifecycle
        {
            let object = read(manifest.ledger, RuntimeObjectKind::OperationLedger)?;
            let ledger = parse_ledger(object.canonical_bytes()).map_err(&error)?;
            for obligation in obligations {
                let mut current = ledger.operation_head;
                let mut visited = BTreeSet::new();
                let request = loop {
                    let address =
                        current.ok_or_else(|| error(CheckpointBundleError::MissingObject))?;
                    if !visited.insert(address) || visited.len() > ledger.operation_count {
                        return Err(error(CheckpointBundleError::InvalidTopology));
                    }
                    let object = read(address, RuntimeObjectKind::OperationNode)?;
                    let node = parse_operation_node(object.canonical_bytes()).map_err(&error)?;
                    match obligation.operation_id().cmp(&node.key) {
                        Ordering::Less => current = node.left,
                        Ordering::Greater => current = node.right,
                        Ordering::Equal => {
                            let object = read(node.record, RuntimeObjectKind::OperationRecord)?;
                            let wire = OperationRecordWire::decode(object.canonical_bytes())
                                .map_err(|e| error(e.into()))?;
                            if wire.operation_id() != obligation.operation_id() {
                                return Err(error(CheckpointBundleError::InvalidTopology));
                            }
                            let digest = wire.request_component_digest();
                            let address = component_object_address(digest).map_err(&error)?;
                            let object = read(address, RuntimeObjectKind::ReconstructiveComponent)?;
                            let request =
                                canonical_component(object.canonical_bytes()).map_err(&error)?;
                            if request.kind() != crate::manifest::RuntimeComponentKind::Operation {
                                return Err(error(RuntimeManifestError::ComponentKind.into()));
                            }
                            break request.integrity();
                        }
                    }
                };
                requests.push((obligation.operation_id(), request));
            }
        }
        Ok(RuntimeCheckpointInspection {
            topology: metadata.topology,
            lifecycle: metadata.lifecycle,
            durability: metadata.durability,
            requests,
        })
    }

    /// Splits a checkpoint into a canonical closed object graph.
    ///
    /// A validated prior bundle is optional and supplies immutable allocations
    /// and exact retained-growth counters; it never supplies semantic defaults.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the source checkpoint, prior publication, or
    /// resulting content-addressed graph is invalid or noncanonical.
    pub fn bundle(
        checkpoint: &RuntimeCheckpoint,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<CheckpointObjectBundle, CheckpointBundleError> {
        if let Some(prior) = prior {
            return Self::advance(checkpoint, prior);
        }
        let mut builder = BundleBuilder {
            objects: BTreeMap::new(),
            visited: 0,
        };
        let built = builder.checkpoint(checkpoint)?;
        let root = built.root;
        builder.retain_candidate_closure(root, None)?;
        let objects: Arc<[RuntimeObject]> =
            builder.objects.into_values().collect::<Vec<_>>().into();
        let retained_bytes = objects
            .iter()
            .map(RuntimeObject::retained_bytes)
            .fold(0_usize, usize::saturating_add);
        if objects.len() > MAX_CHECKPOINT_OBJECTS || retained_bytes > MAX_CHECKPOINT_OBJECT_BYTES {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        let (cas, _, index_nodes) = cas_insert_all(None, &objects)?;
        let stats = CheckpointBundleStats {
            object_count: objects.len(),
            retained_bytes,
            new_object_count: objects.len(),
            new_retained_bytes: retained_bytes,
            visited_object_count: builder.visited,
            new_index_node_count: index_nodes,
            ..CheckpointBundleStats::default()
        };
        let objects_len = objects.len();
        let layers = Arc::new(ObjectLayer {
            prior: None,
            objects: objects.clone(),
        });
        let bundle = CheckpointObjectBundle {
            checkpoint_id: checkpoint.manifest().topology().checkpoint_id,
            checkpoint_integrity: checkpoint.integrity(),
            base_root: None,
            root,
            objects,
            layers,
            cas,
            checkpoint: Some(Arc::new(checkpoint.clone())),
            reuse: Some(Arc::new(built.reuse)),
            stats,
            depth: 1,
            trim_floor: objects_len,
        };
        Ok(bundle)
    }

    /// Incrementally publishes one checkpoint over an exact prior root.
    ///
    /// The ordinary path consumes allocation-backed history suffixes or bounded
    /// replacements plus authenticated command/operation mutations. Non-tail
    /// operation updates rebuild only one balanced-map path. It never scans the
    /// prior closure.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the supplied prior checkpoint is not the exact
    /// semantic ancestor or if a non-incremental ledger rewrite is supplied.
    pub fn advance(
        checkpoint: &RuntimeCheckpoint,
        prior: &CheckpointObjectBundle,
    ) -> Result<CheckpointObjectBundle, CheckpointBundleError> {
        let prior_checkpoint = prior.validated_checkpoint()?;
        let prior_reuse = prior
            .reuse
            .as_deref()
            .ok_or(CheckpointBundleError::IncrementalBaseMismatch)?;
        let mut builder = BundleBuilder {
            objects: BTreeMap::new(),
            visited: 0,
        };
        let built = builder.advance(checkpoint, &prior_checkpoint, prior, prior_reuse)?;
        builder.retain_candidate_closure(built.root, Some(prior))?;
        let candidates = builder.objects.into_values().collect::<Vec<_>>();
        let mut supplied = Vec::with_capacity(candidates.len());
        for object in candidates {
            if let Some(existing) = prior.lookup(object.address) {
                if existing != &object {
                    return Err(CheckpointBundleError::AddressConflict);
                }
            } else {
                supplied.push(object);
            }
        }
        let retained_bytes = supplied
            .iter()
            .map(RuntimeObject::retained_bytes)
            .fold(0_usize, usize::saturating_add);
        if prior.stats.object_count.saturating_add(supplied.len()) > MAX_CHECKPOINT_OBJECTS
            || prior.stats.retained_bytes.saturating_add(retained_bytes)
                > MAX_CHECKPOINT_OBJECT_BYTES
        {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        let objects: Arc<[RuntimeObject]> = supplied.into();
        let (cas, inserted, index_nodes) = cas_insert_all(prior.cas.clone(), &objects)?;
        if inserted != objects.len() {
            return Err(CheckpointBundleError::AddressConflict);
        }
        let layers = Arc::new(ObjectLayer {
            prior: Some(prior.layers.clone()),
            objects: objects.clone(),
        });
        Ok(CheckpointObjectBundle {
            checkpoint_id: checkpoint.manifest().topology().checkpoint_id,
            checkpoint_integrity: checkpoint.integrity(),
            base_root: Some(prior.root),
            root: built.root,
            objects: objects.clone(),
            layers,
            cas,
            checkpoint: Some(Arc::new(checkpoint.clone())),
            reuse: Some(Arc::new(built.reuse)),
            stats: CheckpointBundleStats {
                object_count: prior.stats.object_count.saturating_add(objects.len()),
                retained_bytes: prior.stats.retained_bytes.saturating_add(retained_bytes),
                reused_object_count: prior.stats.object_count,
                reused_retained_bytes: prior.stats.retained_bytes,
                new_object_count: objects.len(),
                new_retained_bytes: retained_bytes,
                visited_object_count: builder.visited,
                new_index_node_count: index_nodes,
            },
            depth: prior.depth.saturating_add(1),
            trim_floor: prior.trim_floor,
        })
    }

    /// Validates the graph and materializes the exact semantic checkpoint.
    /// Object reconstruction has already verified each immutable byte allocation
    /// against its address; materialization does not hash it again.
    ///
    /// # Errors
    ///
    /// Returns a typed error for missing, extra, corrupt, wrongly typed,
    /// noncanonical, cyclic, oversized, or portable-disagreeing object data.
    pub fn materialize(
        bundle: &CheckpointObjectBundle,
    ) -> Result<RuntimeCheckpoint, CheckpointBundleError> {
        let index = ObjectIndex::validate(bundle)?;
        materialize_checkpoint(bundle, &index)
    }
}

fn materialize_checkpoint(
    bundle: &CheckpointObjectBundle,
    index: &ObjectIndex<'_>,
) -> Result<RuntimeCheckpoint, CheckpointBundleError> {
    let root = index.object(bundle.root, RuntimeObjectKind::CheckpointRoot)?;
    let (checkpoint_id, checkpoint_integrity, manifest_address) =
        parse_checkpoint_root(&root.canonical_bytes)?;
    if checkpoint_id != bundle.checkpoint_id || checkpoint_integrity != bundle.checkpoint_integrity
    {
        return Err(CheckpointBundleError::SemanticDisagreement);
    }
    let manifest_root = index.object(manifest_address, RuntimeObjectKind::ManifestRoot)?;
    let root = parse_manifest_root(&manifest_root.canonical_bytes)?;
    let metadata_object = index.object(root.metadata, RuntimeObjectKind::ManifestMetadata)?;
    let metadata = decode_bundle_metadata(&metadata_object.canonical_bytes)?;
    let harness = materialize_component(index, root.harness)?;
    let provider_configuration_object = index.object(
        root.provider_configuration,
        RuntimeObjectKind::ProviderConfiguration,
    )?;
    let provider_configuration =
        canonical_provider_configuration(&provider_configuration_object.canonical_bytes)?;
    let canonical = materialize_canonical(index, root.canonical)?;
    let binding_object = index.object(root.binding, RuntimeObjectKind::ModelRouteBinding)?;
    let binding = canonical_binding(&binding_object.canonical_bytes)?;
    let tools_object = index.object(root.tools, RuntimeObjectKind::ToolRuntime)?;
    let tools = canonical_tools(&tools_object.canonical_bytes)?;
    let ledger = materialize_ledger(index, root.ledger)?;
    let family_artifact = root
        .family
        .map(|address| {
            let object = index.object(address, RuntimeObjectKind::FamilyArtifact)?;
            canonical_family_artifact(&object.canonical_bytes)
        })
        .transpose()?;
    let manifest = assemble_bundle_manifest(
        metadata,
        harness,
        provider_configuration,
        canonical,
        binding,
        tools,
        ledger,
        family_artifact,
        crate::manifest::NamedState::from_values(
            root.named_state
                .into_iter()
                .map(|(key, address)| {
                    let object = index.object(address, RuntimeObjectKind::NamedStateValue)?;
                    Ok((key, object.canonical_bytes.clone()))
                })
                .collect::<Result<_, CheckpointBundleError>>()?,
        )?,
    )?;
    if manifest.topology().checkpoint_id != checkpoint_id {
        return Err(CheckpointBundleError::SemanticDisagreement);
    }
    let checkpoint = RuntimeCheckpoint::new(manifest)?;
    if checkpoint.integrity() != checkpoint_integrity {
        return Err(CheckpointBundleError::SemanticDisagreement);
    }
    Ok(checkpoint)
}

fn materialize_canonical(
    index: &ObjectIndex<'_>,
    address: RuntimeObjectAddress,
) -> Result<RuntimeCanonicalState, CheckpointBundleError> {
    let object = index.object(address, RuntimeObjectKind::CanonicalState)?;
    let spec = parse_canonical(&object.canonical_bytes)?;
    let prefix_object = index.object(spec.prefix, RuntimeObjectKind::StablePrefix)?;
    let prefix = canonical_stable_prefix(&prefix_object.canonical_bytes)?;
    let history = materialize_history(index, spec.history_count, spec.history_head)?;
    if history.identity() != spec.history_identity {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    let working = materialize_working(index, spec.working)?;
    let context = materialize_context(index, spec.context)?;
    RuntimeCanonicalState::new(prefix, history, working, context).map_err(Into::into)
}

fn materialize_history(
    index: &ObjectIndex<'_>,
    count: usize,
    mut head: Option<RuntimeObjectAddress>,
) -> Result<CanonicalHistory, CheckpointBundleError> {
    if (count == 0) != head.is_none() {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    let mut reverse = Vec::with_capacity(count);
    let mut expected = count;
    while let Some(address) = head {
        let object = index.object(address, RuntimeObjectKind::HistoryNode)?;
        let node = parse_sequence_node(&object.canonical_bytes)?;
        if node.count != expected || expected == 0 {
            return Err(CheckpointBundleError::InvalidTopology);
        }
        reverse.push(
            index
                .decoded
                .get(&node.value)
                .ok_or(CheckpointBundleError::MissingObject)?
                .clone(),
        );
        head = node.previous;
        expected = expected.saturating_sub(1);
    }
    if expected != 0 {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    reverse.reverse();
    CanonicalHistory::new(reverse).map_err(|_| CheckpointBundleError::InvalidTopology)
}

fn materialize_working(
    index: &ObjectIndex<'_>,
    address: RuntimeObjectAddress,
) -> Result<WorkingProjection, CheckpointBundleError> {
    let object = index.object(address, RuntimeObjectKind::WorkingProjection)?;
    let spec = parse_working(&object.canonical_bytes)?;
    let segments = spec
        .segments
        .into_iter()
        .map(|address| {
            index
                .decoded
                .get(&address)
                .cloned()
                .ok_or(CheckpointBundleError::MissingObject)
        })
        .collect::<Result<Vec<_>, _>>()?;
    WorkingProjection::new(segments, spec.limits)
        .map_err(|_| CheckpointBundleError::InvalidTopology)
}

fn materialize_component(
    index: &ObjectIndex<'_>,
    address: RuntimeObjectAddress,
) -> Result<ReconstructiveComponent, CheckpointBundleError> {
    let object = index.object(address, RuntimeObjectKind::ReconstructiveComponent)?;
    canonical_component(&object.canonical_bytes)
}

fn materialize_context(
    index: &ObjectIndex<'_>,
    address: RuntimeObjectAddress,
) -> Result<RuntimeContextState, CheckpointBundleError> {
    let object = index.object(address, RuntimeObjectKind::ContextState)?;
    RuntimeContextState::decode(&object.canonical_bytes).map_err(Into::into)
}

fn materialize_ledger(
    index: &ObjectIndex<'_>,
    address: RuntimeObjectAddress,
) -> Result<RuntimeOperationLedger, CheckpointBundleError> {
    let object = index.object(address, RuntimeObjectKind::OperationLedger)?;
    let spec = parse_ledger(&object.canonical_bytes)?;
    let commands = materialize_sequence(
        index,
        spec.command_count,
        spec.command_head,
        RuntimeObjectKind::CommandNode,
        RuntimeObjectKind::CommandRecord,
    )?
    .into_iter()
    .map(|object| canonical_command(&object.canonical_bytes))
    .collect::<Result<Vec<_>, _>>()?;
    if commands
        .windows(2)
        .any(|pair| pair[0].sequence() >= pair[1].sequence())
    {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    let operations = materialize_operation_tree(index, spec.operation_count, spec.operation_head)?
        .into_iter()
        .map(|object| materialize_operation(index, object))
        .collect::<Result<Vec<_>, _>>()?;
    if operations
        .windows(2)
        .any(|pair| pair[0].operation_id() >= pair[1].operation_id())
    {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    RuntimeOperationLedger::new(commands, operations)?
        .with_retired_attempts(spec.retired_local_tool_attempts)
        .map_err(Into::into)
}

fn materialize_sequence<'a>(
    index: &'a ObjectIndex<'a>,
    count: usize,
    mut head: Option<RuntimeObjectAddress>,
    node_kind: RuntimeObjectKind,
    value_kind: RuntimeObjectKind,
) -> Result<Vec<&'a RuntimeObject>, CheckpointBundleError> {
    if (count == 0) != head.is_none() {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    let mut reverse = Vec::with_capacity(count);
    let mut expected = count;
    while let Some(address) = head {
        let node_object = index.object(address, node_kind)?;
        let node = parse_sequence_node(&node_object.canonical_bytes)?;
        if node.count != expected || expected == 0 {
            return Err(CheckpointBundleError::InvalidTopology);
        }
        reverse.push(index.object(node.value, value_kind)?);
        head = node.previous;
        expected = expected.saturating_sub(1);
    }
    if expected != 0 {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    reverse.reverse();
    Ok(reverse)
}

/// Rebuilds one operation record from its referenced component objects; a
/// missing or wrong-kind component fails closed.
fn materialize_operation(
    index: &ObjectIndex<'_>,
    object: &RuntimeObject,
) -> Result<DurableOperationRecord, CheckpointBundleError> {
    canonical_operation_wire(&object.canonical_bytes)?
        .resolve(|digest| materialize_component(index, component_object_address(digest)?))
}
