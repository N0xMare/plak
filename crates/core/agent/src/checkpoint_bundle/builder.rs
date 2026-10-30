//! Deduplicating object-graph builder with ancestry reuse.

use std::collections::BTreeSet;

use plakous_model::{HistorySegment, WorkingProjection};

use crate::RuntimeCheckpoint;
use crate::manifest::{
    DurableCommandRecord, DurableOperationRecord, FamilyArtifact, ModelRouteBinding,
    OperationRecordWire, ReconstructiveComponent, RuntimeCanonicalState, RuntimeOperationLedger,
    ToolRuntimeManifest, encode_bundle_binding, encode_bundle_command_record,
    encode_bundle_component, encode_bundle_family_artifact, encode_bundle_history_segment,
    encode_bundle_metadata, encode_bundle_provider_configuration, encode_bundle_stable_prefix,
    encode_bundle_tools,
};

use super::object::{canonical_history_segment, object_address};
use super::wire::{
    BundleWriter, parse_canonical, parse_checkpoint_root, parse_ledger, parse_manifest_root,
};
use super::{
    BuiltCheckpoint, BundleBuilder, BundleReuse, CheckpointBundleError, CheckpointObjectBundle,
    MAX_CHECKPOINT_OBJECTS, RuntimeObject, RuntimeObjectAddress, RuntimeObjectKind,
};

struct BuiltCanonical {
    address: RuntimeObjectAddress,
    prefix: RuntimeObjectAddress,
    history_head: Option<RuntimeObjectAddress>,
    working: RuntimeObjectAddress,
    context: RuntimeObjectAddress,
}

struct BuiltLedger {
    address: RuntimeObjectAddress,
    command_count: usize,
    command_head: Option<RuntimeObjectAddress>,
    operation_count: usize,
    operation_head: Option<RuntimeObjectAddress>,
}

impl BundleBuilder {
    fn named_state(
        &mut self,
        state: &crate::manifest::NamedState,
        writer: &mut BundleWriter,
        children: &mut Vec<RuntimeObjectAddress>,
    ) -> Result<(), CheckpointBundleError> {
        writer.count(state.values().len())?;
        for (key, value) in state.values() {
            let address = self.insert(
                RuntimeObjectKind::NamedStateValue,
                value.to_vec(),
                Vec::new(),
            )?;
            writer.raw(key.as_bytes())?;
            writer.address(address);
            children.push(address);
        }
        Ok(())
    }

    pub(super) fn retain_candidate_closure(
        &mut self,
        root: RuntimeObjectAddress,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<(), CheckpointBundleError> {
        // Batched tree rewrites leave intermediate parents whose children may
        // have been superseded. Publish only the final root's new DAG, stopping
        // at prior CAS membership rather than scanning its retained closure.
        let mut reachable = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(address) = pending.pop() {
            if !reachable.insert(address) {
                continue;
            }
            if let Some(object) = self.objects.get(&address) {
                pending.extend_from_slice(object.children());
            } else if prior.and_then(|bundle| bundle.lookup(address)).is_none() {
                return Err(CheckpointBundleError::MissingObject);
            }
        }
        self.objects
            .retain(|address, _| reachable.contains(address));
        debug_assert!(self.objects.values().all(|object| {
            object.children().iter().all(|child| {
                self.objects.contains_key(child)
                    || prior.and_then(|bundle| bundle.lookup(*child)).is_some()
            })
        }));
        Ok(())
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "changed canonical bytes are transferred into the immutable object without cloning"
    )]
    fn reuse_semantic(
        &mut self,
        kind: RuntimeObjectKind,
        bytes: Vec<u8>,
        prior: RuntimeObjectAddress,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        self.visited = self.visited.saturating_add(1);
        let address = object_address(kind, &bytes)?;
        if address == prior {
            Ok(prior)
        } else {
            self.insert(kind, bytes, Vec::new())
        }
    }

    fn reuse_or_build<F>(
        &mut self,
        kind: RuntimeObjectKind,
        bytes: &[u8],
        prior: RuntimeObjectAddress,
        build: F,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError>
    where
        F: FnOnce(&mut Self) -> Result<RuntimeObjectAddress, CheckpointBundleError>,
    {
        self.visited = self.visited.saturating_add(1);
        if object_address(kind, bytes)? == prior {
            Ok(prior)
        } else {
            build(self)
        }
    }

    pub(super) fn checkpoint(
        &mut self,
        checkpoint: &RuntimeCheckpoint,
    ) -> Result<BuiltCheckpoint, CheckpointBundleError> {
        let manifest = checkpoint.manifest();
        let metadata = self.insert(
            RuntimeObjectKind::ManifestMetadata,
            encode_bundle_metadata(manifest)?,
            Vec::new(),
        )?;
        let harness = self.component(manifest.harness_plan_component())?;
        let provider_configuration = self.insert(
            RuntimeObjectKind::ProviderConfiguration,
            encode_bundle_provider_configuration(manifest.provider_configuration())?,
            Vec::new(),
        )?;
        let canonical = self.canonical(manifest.canonical())?;
        let binding = self.binding(manifest.binding())?;
        let tools = self.tools(manifest.tools())?;
        let ledger = self.ledger(manifest.ledger())?;
        let family = manifest
            .family_artifact()
            .map(|artifact| self.family_artifact(artifact))
            .transpose()?;

        let mut manifest_writer = BundleWriter::default();
        for address in [
            metadata,
            harness,
            provider_configuration,
            canonical,
            binding,
            tools,
            ledger,
        ] {
            manifest_writer.address(address);
        }
        manifest_writer.optional_address(family);
        let mut manifest_children = vec![
            metadata,
            harness,
            provider_configuration,
            canonical,
            binding,
            tools,
            ledger,
        ];
        manifest_children.extend(family);
        self.named_state(
            manifest.named_state(),
            &mut manifest_writer,
            &mut manifest_children,
        )?;
        let manifest_root = self.insert(
            RuntimeObjectKind::ManifestRoot,
            manifest_writer.finish(),
            manifest_children,
        )?;

        let mut root_writer = BundleWriter::default();
        root_writer.uuid(manifest.topology().checkpoint_id.as_uuid());
        root_writer.digest(checkpoint.integrity())?;
        root_writer.address(manifest_root);
        let root = self.insert(
            RuntimeObjectKind::CheckpointRoot,
            root_writer.finish(),
            vec![manifest_root],
        )?;
        let canonical_spec = parse_canonical(
            &self
                .objects
                .get(&canonical)
                .ok_or(CheckpointBundleError::MissingObject)?
                .canonical_bytes,
        )?;
        let ledger_spec = parse_ledger(
            &self
                .objects
                .get(&ledger)
                .ok_or(CheckpointBundleError::MissingObject)?
                .canonical_bytes,
        )?;
        Ok(BuiltCheckpoint {
            root,
            reuse: BundleReuse {
                canonical,
                harness,
                provider_configuration,
                prefix: canonical_spec.prefix,
                history_head: canonical_spec.history_head,
                working: canonical_spec.working,
                context: canonical_spec.context,
                binding,
                tools,
                command_count: ledger_spec.command_count,
                command_head: ledger_spec.command_head,
                operation_count: ledger_spec.operation_count,
                operation_head: ledger_spec.operation_head,
                ledger,
                family,
                named_state: manifest.named_state().addresses().clone(),
            },
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Incremental bundle construction traverses the pending child graph while applying one bounded builder step"
    )]
    pub(super) fn advance(
        &mut self,
        checkpoint: &RuntimeCheckpoint,
        prior_checkpoint: &RuntimeCheckpoint,
        prior_bundle: &CheckpointObjectBundle,
        prior: &BundleReuse,
    ) -> Result<BuiltCheckpoint, CheckpointBundleError> {
        let manifest = checkpoint.manifest();
        let prior_manifest = prior_checkpoint.manifest();
        let metadata = self.insert(
            RuntimeObjectKind::ManifestMetadata,
            encode_bundle_metadata(manifest)?,
            Vec::new(),
        )?;
        let harness = self.reuse_semantic(
            RuntimeObjectKind::ReconstructiveComponent,
            encode_bundle_component(manifest.harness_plan_component())?,
            prior.harness,
        )?;
        let provider_configuration = self.reuse_semantic(
            RuntimeObjectKind::ProviderConfiguration,
            encode_bundle_provider_configuration(manifest.provider_configuration())?,
            prior.provider_configuration,
        )?;
        let canonical = self.advance_canonical(
            manifest.canonical(),
            prior_manifest.canonical(),
            prior_bundle,
            prior,
        )?;
        let binding = self.reuse_or_build(
            RuntimeObjectKind::ModelRouteBinding,
            &encode_bundle_binding(manifest.binding())?,
            prior.binding,
            |builder| builder.binding(manifest.binding()),
        )?;
        let tools = self.reuse_or_build(
            RuntimeObjectKind::ToolRuntime,
            &encode_bundle_tools(manifest.tools())?,
            prior.tools,
            |builder| builder.tools(manifest.tools()),
        )?;
        let ledger = self.advance_ledger(
            manifest.ledger(),
            prior_manifest.ledger(),
            prior_bundle,
            prior,
        )?;
        let family = match (manifest.family_artifact(), prior.family) {
            (Some(artifact), Some(prior_address)) => Some(self.reuse_or_build(
                RuntimeObjectKind::FamilyArtifact,
                &encode_bundle_family_artifact(artifact)?,
                prior_address,
                |builder| builder.family_artifact(artifact),
            )?),
            (Some(artifact), None) => Some(self.family_artifact(artifact)?),
            (None, _) => None,
        };

        let mut manifest_writer = BundleWriter::default();
        for address in [
            metadata,
            harness,
            provider_configuration,
            canonical.address,
            binding,
            tools,
            ledger.address,
        ] {
            manifest_writer.address(address);
        }
        manifest_writer.optional_address(family);
        let mut manifest_children = vec![
            metadata,
            harness,
            provider_configuration,
            canonical.address,
            binding,
            tools,
            ledger.address,
        ];
        manifest_children.extend(family);
        if manifest.named_state().addresses() == &prior.named_state {
            manifest_writer.count(prior.named_state.len())?;
            for (key, address) in &prior.named_state {
                manifest_writer.raw(key.as_bytes())?;
                manifest_writer.address(*address);
                manifest_children.push(*address);
            }
        } else {
            self.named_state(
                manifest.named_state(),
                &mut manifest_writer,
                &mut manifest_children,
            )?;
        }
        let manifest_root = self.insert(
            RuntimeObjectKind::ManifestRoot,
            manifest_writer.finish(),
            manifest_children,
        )?;
        let mut root_writer = BundleWriter::default();
        root_writer.uuid(manifest.topology().checkpoint_id.as_uuid());
        root_writer.digest(checkpoint.integrity())?;
        root_writer.address(manifest_root);
        let root = self.insert(
            RuntimeObjectKind::CheckpointRoot,
            root_writer.finish(),
            vec![manifest_root],
        )?;
        Ok(BuiltCheckpoint {
            root,
            reuse: BundleReuse {
                canonical: canonical.address,
                harness,
                provider_configuration,
                prefix: canonical.prefix,
                history_head: canonical.history_head,
                working: canonical.working,
                context: canonical.context,
                binding,
                tools,
                command_count: ledger.command_count,
                command_head: ledger.command_head,
                operation_count: ledger.operation_count,
                operation_head: ledger.operation_head,
                ledger: ledger.address,
                family,
                named_state: manifest.named_state().addresses().clone(),
            },
        })
    }

    fn canonical(
        &mut self,
        canonical: &RuntimeCanonicalState,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let prefix = self.insert(
            RuntimeObjectKind::StablePrefix,
            encode_bundle_stable_prefix(canonical.stable_prefix())?,
            Vec::new(),
        )?;
        let mut history_head = None;
        for (index, segment) in canonical.history().segments().iter().enumerate() {
            let segment = self.segment(segment)?;
            let count = index.saturating_add(1);
            let mut writer = BundleWriter::default();
            writer.count(count)?;
            writer.optional_address(history_head);
            writer.address(segment);
            let mut children = Vec::with_capacity(2);
            children.extend(history_head);
            children.push(segment);
            history_head =
                Some(self.insert(RuntimeObjectKind::HistoryNode, writer.finish(), children)?);
        }
        let working = self.working(canonical.working())?;
        let context = self.insert(
            RuntimeObjectKind::ContextState,
            canonical.context().encode()?,
            Vec::new(),
        )?;
        let mut writer = BundleWriter::default();
        writer.address(prefix);
        writer.count(canonical.history().segments().len())?;
        writer.digest(canonical.history().identity())?;
        writer.optional_address(history_head);
        writer.address(working);
        writer.address(context);
        let mut children = vec![prefix];
        children.extend(history_head);
        children.extend([working, context]);
        self.insert(RuntimeObjectKind::CanonicalState, writer.finish(), children)
    }

    fn advance_canonical(
        &mut self,
        canonical: &RuntimeCanonicalState,
        prior_canonical: &RuntimeCanonicalState,
        _prior_bundle: &CheckpointObjectBundle,
        prior: &BundleReuse,
    ) -> Result<BuiltCanonical, CheckpointBundleError> {
        let prefix = self.reuse_semantic(
            RuntimeObjectKind::StablePrefix,
            encode_bundle_stable_prefix(canonical.stable_prefix())?,
            prior.prefix,
        )?;
        let history_delta = canonical
            .history()
            .append_delta_since(prior_canonical.history());
        let unchanged = history_delta.is_none()
            && canonical.history().segments().len() == prior_canonical.history().segments().len()
            && canonical.history().identity() == prior_canonical.history().identity();
        let (mut history_head, start, segments) = match history_delta {
            Some(delta) => (
                prior.history_head,
                prior_canonical.history().segments().len(),
                delta,
            ),
            None if unchanged => (
                prior.history_head,
                canonical.history().segments().len(),
                Vec::new(),
            ),
            None => (None, 0, canonical.history().segments().to_vec()),
        };
        for (offset, segment) in segments.iter().enumerate() {
            let segment_address = self.segment(segment)?;
            let count = start.saturating_add(offset).saturating_add(1);
            let mut writer = BundleWriter::default();
            writer.count(count)?;
            writer.optional_address(history_head);
            writer.address(segment_address);
            let mut children = Vec::with_capacity(2);
            children.extend(history_head);
            children.push(segment_address);
            history_head =
                Some(self.insert(RuntimeObjectKind::HistoryNode, writer.finish(), children)?);
        }
        let working = if canonical.working() == prior_canonical.working() {
            self.visited = self.visited.saturating_add(1);
            prior.working
        } else {
            self.working(canonical.working())?
        };
        let context = self.reuse_semantic(
            RuntimeObjectKind::ContextState,
            canonical.context().encode()?,
            prior.context,
        )?;
        let mut writer = BundleWriter::default();
        writer.address(prefix);
        writer.count(canonical.history().segments().len())?;
        writer.digest(canonical.history().identity())?;
        writer.optional_address(history_head);
        writer.address(working);
        writer.address(context);
        let mut children = vec![prefix];
        children.extend(history_head);
        children.extend([working, context]);
        let bytes = writer.finish();
        let address =
            if object_address(RuntimeObjectKind::CanonicalState, &bytes)? == prior.canonical {
                self.visited = self.visited.saturating_add(1);
                prior.canonical
            } else {
                self.insert(RuntimeObjectKind::CanonicalState, bytes, children)?
            };
        Ok(BuiltCanonical {
            address,
            prefix,
            history_head,
            working,
            context,
        })
    }

    fn advance_ledger(
        &mut self,
        ledger: &RuntimeOperationLedger,
        prior_ledger: &RuntimeOperationLedger,
        prior_bundle: &CheckpointObjectBundle,
        prior: &BundleReuse,
    ) -> Result<BuiltLedger, CheckpointBundleError> {
        let (Some(command_delta), Some(operation_delta)) = (
            ledger.command_delta_since(prior_ledger),
            ledger.operation_delta_since(prior_ledger),
        ) else {
            // A ledger that dropped finalized records is rebuilt whole; the
            // records it kept dedupe against the prior closure.
            if !ledger.is_retained_successor_of(prior_ledger) {
                return Err(CheckpointBundleError::IncrementalBaseMismatch);
            }
            return self.full_ledger(ledger);
        };
        let mut command_head = prior.command_head;
        let mut command_count = prior.command_count;
        for record in command_delta {
            let record = self.command_record(record)?;
            command_count = command_count.saturating_add(1);
            command_head = Some(self.sequence_node(
                RuntimeObjectKind::CommandNode,
                command_count,
                command_head,
                record,
            )?);
        }
        let mut operation_head = prior.operation_head;
        let mut operation_count = prior.operation_count;
        for mutation in operation_delta {
            let expected_insert = mutation.inserted();
            let record = mutation.record();
            let key = record.operation_id();
            let record = self.operation_record(record)?;
            let (root, inserted) =
                self.operation_tree_upsert(operation_head, key, record, Some(prior_bundle))?;
            if inserted != expected_insert {
                return Err(CheckpointBundleError::IncrementalBaseMismatch);
            }
            operation_count = operation_count.saturating_add(usize::from(inserted));
            operation_head = Some(root);
        }
        let mut writer = BundleWriter::default();
        writer.count(command_count)?;
        writer.optional_address(command_head);
        writer.count(operation_count)?;
        writer.optional_address(operation_head);
        let mut children = Vec::with_capacity(2);
        children.extend(command_head);
        children.extend(operation_head);
        let bytes = writer.finish();
        let address = if object_address(RuntimeObjectKind::OperationLedger, &bytes)? == prior.ledger
        {
            self.visited = self.visited.saturating_add(1);
            prior.ledger
        } else {
            self.insert(RuntimeObjectKind::OperationLedger, bytes, children)?
        };
        Ok(BuiltLedger {
            address,
            command_count,
            command_head,
            operation_count,
            operation_head,
        })
    }

    pub(super) fn working(
        &mut self,
        working: &WorkingProjection,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let limits = working.limits();
        let mut writer = BundleWriter::default();
        writer.usize(limits.max_segments())?;
        writer.usize(limits.max_items())?;
        writer.usize(limits.max_retained_bytes())?;
        writer.count(working.segments().len())?;
        let mut children = Vec::with_capacity(working.segments().len());
        for segment in working.segments() {
            let address = self.segment(segment)?;
            writer.address(address);
            children.push(address);
        }
        self.insert(
            RuntimeObjectKind::WorkingProjection,
            writer.finish(),
            children,
        )
    }

    pub(super) fn segment(
        &mut self,
        segment: &HistorySegment,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let address = self.insert(
            RuntimeObjectKind::HistorySegment,
            encode_bundle_history_segment(segment)?,
            Vec::new(),
        )?;
        let object = self
            .objects
            .get_mut(&address)
            .ok_or(CheckpointBundleError::MissingObject)?;
        if object.decoded_segment.as_ref() != Some(segment) {
            return Err(CheckpointBundleError::NonCanonical);
        }
        object.decoded_segment = Some(segment.clone());
        Ok(address)
    }

    pub(super) fn component(
        &mut self,
        component: &ReconstructiveComponent,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        self.insert(
            RuntimeObjectKind::ReconstructiveComponent,
            encode_bundle_component(component)?,
            Vec::new(),
        )
    }

    fn binding(
        &mut self,
        binding: &ModelRouteBinding,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let descriptor = self.component(binding.descriptor())?;
        let evidence = self.component(binding.admission().evidence())?;
        self.insert(
            RuntimeObjectKind::ModelRouteBinding,
            encode_bundle_binding(binding)?,
            vec![descriptor, evidence],
        )
    }

    fn tools(
        &mut self,
        tools: &ToolRuntimeManifest,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let children = [
            tools.registry(),
            tools.exposure(),
            tools.environment(),
            tools.effect_policy(),
        ]
        .into_iter()
        .map(|component| self.component(component))
        .collect::<Result<Vec<_>, _>>()?;
        self.insert(
            RuntimeObjectKind::ToolRuntime,
            encode_bundle_tools(tools)?,
            children,
        )
    }

    fn ledger(
        &mut self,
        ledger: &RuntimeOperationLedger,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        Ok(self.full_ledger(ledger)?.address)
    }

    fn full_ledger(
        &mut self,
        ledger: &RuntimeOperationLedger,
    ) -> Result<BuiltLedger, CheckpointBundleError> {
        let mut command_head = None;
        for (index, record) in ledger.commands().into_iter().enumerate() {
            let record = self.command_record(record)?;
            command_head = Some(self.sequence_node(
                RuntimeObjectKind::CommandNode,
                index.saturating_add(1),
                command_head,
                record,
            )?);
        }
        let mut operation_head = None;
        for record in ledger.operations() {
            let key = record.operation_id();
            let record = self.operation_record(record)?;
            let (root, inserted) = self.operation_tree_upsert(operation_head, key, record, None)?;
            if !inserted {
                return Err(CheckpointBundleError::InvalidTopology);
            }
            operation_head = Some(root);
        }
        let mut writer = BundleWriter::default();
        writer.count(ledger.command_count())?;
        writer.optional_address(command_head);
        writer.count(ledger.operation_count())?;
        writer.optional_address(operation_head);
        let mut children = Vec::with_capacity(2);
        children.extend(command_head);
        children.extend(operation_head);
        let address = self.insert(
            RuntimeObjectKind::OperationLedger,
            writer.finish(),
            children,
        )?;
        Ok(BuiltLedger {
            address,
            command_count: ledger.command_count(),
            command_head,
            operation_count: ledger.operation_count(),
            operation_head,
        })
    }

    fn command_record(
        &mut self,
        record: &DurableCommandRecord,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let component = self.component(record.command_component())?;
        self.insert(
            RuntimeObjectKind::CommandRecord,
            encode_bundle_command_record(record)?,
            vec![component],
        )
    }

    fn operation_record(
        &mut self,
        record: &DurableOperationRecord,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let mut children = Vec::new();
        let wire = OperationRecordWire::new(record, |component| {
            let address = self.component(component)?;
            children.push(address);
            Ok::<_, CheckpointBundleError>(address.digest)
        })?;
        self.insert(RuntimeObjectKind::OperationRecord, wire.encode()?, children)
    }

    fn family_artifact(
        &mut self,
        artifact: &FamilyArtifact,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let binding = self.component(artifact.binding_component())?;
        self.insert(
            RuntimeObjectKind::FamilyArtifact,
            encode_bundle_family_artifact(artifact)?,
            vec![binding],
        )
    }

    fn sequence_node(
        &mut self,
        kind: RuntimeObjectKind,
        count: usize,
        previous: Option<RuntimeObjectAddress>,
        record: RuntimeObjectAddress,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let mut writer = BundleWriter::default();
        writer.count(count)?;
        writer.optional_address(previous);
        writer.address(record);
        let mut children = Vec::with_capacity(2);
        children.extend(previous);
        children.push(record);
        self.insert(kind, writer.finish(), children)
    }

    pub(super) fn insert(
        &mut self,
        kind: RuntimeObjectKind,
        canonical_bytes: Vec<u8>,
        children: Vec<RuntimeObjectAddress>,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        self.visited = self.visited.saturating_add(1);
        let address = object_address(kind, &canonical_bytes)?;
        let mut candidate = RuntimeObject::new_trusted(address, canonical_bytes, children);
        if let Some(existing) = self.objects.get(&address) {
            if existing != &candidate {
                return Err(CheckpointBundleError::AddressConflict);
            }
            return Ok(address);
        }
        if kind == RuntimeObjectKind::HistorySegment {
            // Newly encoded segments still cross the model's validation boundary.
            let segment = canonical_history_segment(&candidate.canonical_bytes)?;
            if encode_bundle_history_segment(&segment)? != candidate.canonical_bytes.as_ref() {
                return Err(CheckpointBundleError::NonCanonical);
            }
            candidate.decoded_segment = Some(segment);
        }
        self.objects.insert(address, candidate);
        if self.objects.len() > MAX_CHECKPOINT_OBJECTS {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        Ok(address)
    }
}

pub(super) fn reuse_from_bundle(
    bundle: &CheckpointObjectBundle,
) -> Result<BundleReuse, CheckpointBundleError> {
    let root = bundle
        .lookup(bundle.root)
        .ok_or(CheckpointBundleError::MissingObject)?;
    if root.address.kind != RuntimeObjectKind::CheckpointRoot {
        return Err(CheckpointBundleError::WrongKind);
    }
    let (_, _, manifest_address) = parse_checkpoint_root(&root.canonical_bytes)?;
    let manifest_object = bundle
        .lookup(manifest_address)
        .ok_or(CheckpointBundleError::MissingObject)?;
    if manifest_object.address.kind != RuntimeObjectKind::ManifestRoot {
        return Err(CheckpointBundleError::WrongKind);
    }
    let manifest = parse_manifest_root(&manifest_object.canonical_bytes)?;
    let canonical_object = bundle
        .lookup(manifest.canonical)
        .ok_or(CheckpointBundleError::MissingObject)?;
    if canonical_object.address.kind != RuntimeObjectKind::CanonicalState {
        return Err(CheckpointBundleError::WrongKind);
    }
    let canonical = parse_canonical(&canonical_object.canonical_bytes)?;
    let ledger_object = bundle
        .lookup(manifest.ledger)
        .ok_or(CheckpointBundleError::MissingObject)?;
    if ledger_object.address.kind != RuntimeObjectKind::OperationLedger {
        return Err(CheckpointBundleError::WrongKind);
    }
    let ledger = parse_ledger(&ledger_object.canonical_bytes)?;
    Ok(BundleReuse {
        canonical: manifest.canonical,
        harness: manifest.harness,
        provider_configuration: manifest.provider_configuration,
        prefix: canonical.prefix,
        history_head: canonical.history_head,
        working: canonical.working,
        context: canonical.context,
        binding: manifest.binding,
        tools: manifest.tools,
        command_count: ledger.command_count,
        command_head: ledger.command_head,
        operation_count: ledger.operation_count,
        operation_head: ledger.operation_head,
        ledger: manifest.ledger,
        family: manifest.family,
        named_state: manifest.named_state,
    })
}
