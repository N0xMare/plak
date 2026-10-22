//! Backend-neutral content-addressed representation of runtime checkpoints.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use plakous_model::{
    CanonicalHistory, CheckpointId, CompiledProviderConfiguration, Digest, DigestAlgorithm,
    DigestDomain, HistorySegment, StableRequestPrefix, WorkingProjection, WorkingProjectionLimits,
};
use thiserror::Error;

use crate::manifest::{
    DurableCommandRecord, DurableOperationRecord, FamilyArtifact, ModelRouteBinding,
    OperationRecordWire, ReconstructiveComponent, RuntimeCanonicalState, RuntimeContextState,
    RuntimeManifestError, RuntimeOperationLedger, ToolRuntimeManifest, assemble_bundle_manifest,
    decode_bundle_binding, decode_bundle_command_record, decode_bundle_component,
    decode_bundle_digest, decode_bundle_family_artifact, decode_bundle_history_segment,
    decode_bundle_metadata, decode_bundle_provider_configuration, decode_bundle_stable_prefix,
    decode_bundle_tools, encode_bundle_binding, encode_bundle_command_record,
    encode_bundle_component, encode_bundle_digest, encode_bundle_family_artifact,
    encode_bundle_history_segment, encode_bundle_metadata, encode_bundle_metadata_value,
    encode_bundle_provider_configuration, encode_bundle_stable_prefix, encode_bundle_tools,
};
use crate::{MAX_RUNTIME_CHECKPOINT_BYTES, MAX_RUNTIME_RECORDS, RuntimeCheckpoint};

mod wire;

use wire::{
    BundleWriter, OperationNodeSpec, parse_canonical, parse_checkpoint_root, parse_ledger,
    parse_manifest_root, parse_operation_node, parse_sequence_node, parse_working,
};

/// Format epoch of the complete persisted checkpoint object graph. Every
/// object address carries it and nothing from another epoch decodes. Any
/// pre-release change to a persisted runtime encoding bumps this one number;
/// the per-component revision constants inside the graph are frozen.
// Epoch 29 isolates native compaction provenance with a retained current-round tail.
pub const RUNTIME_FORMAT_EPOCH: u32 = 29;

const OBJECT_ADDRESS_BYTES: usize = 1 + 4 + 32;
const MAX_CHECKPOINT_OBJECTS: usize = MAX_RUNTIME_RECORDS * 8 + 64;
// A long session retains operation evidence outside its compacted conversation.
// This bound must cover the reachable graph, not just a few flat manifest encodings.
const MAX_CHECKPOINT_OBJECT_BYTES: usize = 1024 * 1024 * 1024;
const OBJECT_ADDRESS_DOMAIN: &[u8] = b"plakous-agent/runtime-object-v3";

/// Closed semantic kind of one immutable checkpoint object.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum RuntimeObjectKind {
    /// Small checkpoint envelope naming the manifest root and semantic integrity.
    CheckpointRoot = 1,
    /// Small manifest root naming every reconstruction group.
    ManifestRoot = 2,
    /// Provenance, topology, lifecycle, and durability scalars.
    ManifestMetadata = 3,
    /// Exact immutable stable request prefix.
    StablePrefix = 4,
    /// One exact canonical history segment.
    HistorySegment = 5,
    /// One append-sharing persistent history-vector node.
    HistoryNode = 6,
    /// Exact bounded working projection over referenced segments.
    WorkingProjection = 7,
    /// One exact externally owned reconstruction component.
    ReconstructiveComponent = 8,
    /// Stable prefix, history head, working projection, and context binding.
    CanonicalState = 9,
    /// Exact model/route/account/profile binding.
    ModelRouteBinding = 10,
    /// Exact tool/exposure/environment/effect-policy binding.
    ToolRuntime = 11,
    /// One exact durable command ledger record.
    CommandRecord = 12,
    /// One append-sharing persistent command-vector node.
    CommandNode = 13,
    /// One exact durable physical-operation record.
    OperationRecord = 14,
    /// One structurally shared authenticated operation-map node.
    OperationNode = 15,
    /// Ordered command and operation vector heads.
    OperationLedger = 16,
    /// Exact family-native artifact and binding.
    FamilyArtifact = 17,
    /// Exact typed runtime context state.
    ContextState = 18,
    /// Reconstruction-complete selected and effective provider configuration.
    ProviderConfiguration = 19,
}

impl RuntimeObjectKind {
    /// Decodes the closed persisted kind tag without delegating codec logic to a backend.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointBundleError::UnknownObjectKind`] for an unknown tag.
    pub fn from_store_tag(tag: u8) -> Result<Self, CheckpointBundleError> {
        match tag {
            1 => Ok(Self::CheckpointRoot),
            2 => Ok(Self::ManifestRoot),
            3 => Ok(Self::ManifestMetadata),
            4 => Ok(Self::StablePrefix),
            5 => Ok(Self::HistorySegment),
            6 => Ok(Self::HistoryNode),
            7 => Ok(Self::WorkingProjection),
            8 => Ok(Self::ReconstructiveComponent),
            9 => Ok(Self::CanonicalState),
            10 => Ok(Self::ModelRouteBinding),
            11 => Ok(Self::ToolRuntime),
            12 => Ok(Self::CommandRecord),
            13 => Ok(Self::CommandNode),
            14 => Ok(Self::OperationRecord),
            15 => Ok(Self::OperationNode),
            16 => Ok(Self::OperationLedger),
            17 => Ok(Self::FamilyArtifact),
            18 => Ok(Self::ContextState),
            19 => Ok(Self::ProviderConfiguration),
            _ => Err(CheckpointBundleError::UnknownObjectKind(tag)),
        }
    }
}

/// Complete content address of one immutable runtime object.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RuntimeObjectAddress {
    kind: RuntimeObjectKind,
    codec_revision: u32,
    digest: Digest,
}

impl RuntimeObjectAddress {
    /// Reconstructs and validates one address read from normalized backend columns.
    ///
    /// # Errors
    ///
    /// Returns an error unless the codec revision and digest algorithm, domain,
    /// and version are exactly those of this fresh object format.
    pub fn from_store_parts(
        kind: RuntimeObjectKind,
        codec_revision: u32,
        digest: Digest,
    ) -> Result<Self, CheckpointBundleError> {
        if codec_revision != RUNTIME_FORMAT_EPOCH
            || digest.algorithm() != DigestAlgorithm::Sha256
            || digest.domain() != DigestDomain::Blob
            || digest.version() != RUNTIME_FORMAT_EPOCH
        {
            return Err(CheckpointBundleError::UnsupportedRevision);
        }
        Ok(Self {
            kind,
            codec_revision,
            digest,
        })
    }

    /// Returns the closed object kind.
    #[must_use]
    pub const fn kind(self) -> RuntimeObjectKind {
        self.kind
    }

    /// Returns the exact object codec revision.
    #[must_use]
    pub const fn codec_revision(self) -> u32 {
        self.codec_revision
    }

    /// Returns the domain-separated content digest.
    #[must_use]
    pub const fn digest(self) -> Digest {
        self.digest
    }
}

impl Ord for RuntimeObjectAddress {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.kind, self.codec_revision, self.digest.bytes()).cmp(&(
            other.kind,
            other.codec_revision,
            other.digest.bytes(),
        ))
    }
}

impl PartialOrd for RuntimeObjectAddress {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for RuntimeObjectAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeObjectAddress")
            .field("kind", &self.kind)
            .field("codec_revision", &self.codec_revision)
            .field("digest", &self.digest)
            .finish()
    }
}

/// One exact immutable object and its indexed ordered child mirror.
#[derive(Clone)]
pub struct RuntimeObject {
    address: RuntimeObjectAddress,
    canonical_bytes: Arc<[u8]>,
    addressed_bytes: Arc<[u8]>,
    children: Arc<[RuntimeObjectAddress]>,
    decoded_segment: Option<HistorySegment>,
}

impl PartialEq for RuntimeObject {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
            && self.canonical_bytes == other.canonical_bytes
            && self.children == other.children
    }
}

impl Eq for RuntimeObject {}

impl RuntimeObject {
    // Only BundleBuilder may bypass reconstruction: it just encoded these
    // bytes and their ordered edges and computed their address.
    fn new_trusted(
        address: RuntimeObjectAddress,
        canonical_bytes: Vec<u8>,
        children: Vec<RuntimeObjectAddress>,
    ) -> Self {
        let canonical_bytes: Arc<[u8]> = canonical_bytes.into();
        Self {
            address,
            addressed_bytes: Arc::clone(&canonical_bytes),
            canonical_bytes,
            children: children.into(),
            decoded_segment: None,
        }
    }

    /// Reconstructs one object read from normalized backend rows.
    ///
    /// This validates the address against the exact canonical bytes. The closed
    /// ordered child mirror is validated with the other objects by
    /// [`CheckpointObjectBundle::from_store`], where child kinds and presence
    /// are available.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign revision/digest shape or bytes whose
    /// recomputed address differs from the stored address.
    pub fn from_store(
        address: RuntimeObjectAddress,
        canonical_bytes: Vec<u8>,
        ordered_children: Vec<RuntimeObjectAddress>,
    ) -> Result<Self, CheckpointBundleError> {
        RuntimeObjectAddress::from_store_parts(
            address.kind,
            address.codec_revision,
            address.digest,
        )?;
        #[cfg(test)]
        ADDRESS_VALIDATIONS.update(|count| count + 1);
        if object_address(address.kind, &canonical_bytes)? != address {
            return Err(CheckpointBundleError::InvalidAddress);
        }
        let decoded_segment = if address.kind == RuntimeObjectKind::HistorySegment {
            Some(canonical_history_segment(&canonical_bytes)?)
        } else {
            None
        };
        let expected_children = if decoded_segment.is_some() {
            Vec::new()
        } else {
            canonical_child_specs(address.kind, &canonical_bytes)?
        }
        .into_iter()
        .map(|(child, _)| child)
        .collect::<Vec<_>>();
        if expected_children != ordered_children {
            return Err(CheckpointBundleError::NonCanonical);
        }
        let canonical_bytes: Arc<[u8]> = canonical_bytes.into();
        Ok(Self {
            address,
            addressed_bytes: Arc::clone(&canonical_bytes),
            canonical_bytes,
            children: ordered_children.into(),
            decoded_segment,
        })
    }

    /// Returns the verified content address.
    #[must_use]
    pub const fn address(&self) -> RuntimeObjectAddress {
        self.address
    }

    /// Returns exact canonical bytes. Digests never substitute for these bytes.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    /// Returns the ordered child-address mirror committed by the canonical bytes.
    #[must_use]
    pub fn children(&self) -> &[RuntimeObjectAddress] {
        &self.children
    }

    /// Returns conservative retained payload and edge bytes for benchmark counters.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.canonical_bytes
            .len()
            .saturating_add(self.children.len().saturating_mul(OBJECT_ADDRESS_BYTES))
    }
}

impl fmt::Debug for RuntimeObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeObject")
            .field("address", &self.address)
            .field("canonical_bytes_len", &self.canonical_bytes.len())
            .field("children", &self.children)
            .finish_non_exhaustive()
    }
}

/// Sharing and retained-growth observations for one publication ancestry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CheckpointBundleStats {
    object_count: usize,
    retained_bytes: usize,
    reused_object_count: usize,
    reused_retained_bytes: usize,
    new_object_count: usize,
    new_retained_bytes: usize,
    visited_object_count: usize,
    new_index_node_count: usize,
}

impl CheckpointBundleStats {
    /// Returns the total unique CAS object count retained through this publication.
    #[must_use]
    pub const fn object_count(self) -> usize {
        self.object_count
    }

    /// Returns total unique CAS object and edge bytes retained through this publication.
    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }

    /// Returns objects whose addresses existed in the supplied prior bundle.
    #[must_use]
    pub const fn reused_object_count(self) -> usize {
        self.reused_object_count
    }

    /// Returns conservatively retained bytes reused from the prior bundle.
    #[must_use]
    pub const fn reused_retained_bytes(self) -> usize {
        self.reused_retained_bytes
    }

    /// Returns objects newly retained relative to the prior bundle.
    #[must_use]
    pub const fn new_object_count(self) -> usize {
        self.new_object_count
    }

    /// Returns conservatively retained bytes newly added relative to the prior bundle.
    #[must_use]
    pub const fn new_retained_bytes(self) -> usize {
        self.new_retained_bytes
    }

    /// Returns semantic objects visited by the split path.
    ///
    /// Ordinary incremental publication keeps this proportional to the delta;
    /// cold full splitting intentionally visits the full reachable closure.
    #[must_use]
    pub const fn visited_object_count(self) -> usize {
        self.visited_object_count
    }

    /// Returns newly allocated persistent CAS-index nodes for this publication.
    ///
    /// Patricia depth is fixed by the address width, so this is a direct bound
    /// on otherwise-hidden publication address metadata.
    #[must_use]
    pub const fn new_index_node_count(self) -> usize {
        self.new_index_node_count
    }
}

/// Small root plus the exact closed object set required to materialize a checkpoint.
#[derive(Clone)]
pub struct CheckpointObjectBundle {
    checkpoint_id: CheckpointId,
    checkpoint_integrity: Digest,
    base_root: Option<RuntimeObjectAddress>,
    root: RuntimeObjectAddress,
    objects: Arc<[RuntimeObject]>,
    layers: Arc<ObjectLayer>,
    cas: Option<Arc<CasNode>>,
    checkpoint: Option<Arc<RuntimeCheckpoint>>,
    reuse: Option<Arc<BundleReuse>>,
    stats: CheckpointBundleStats,
    /// Layers chained since the bundle was last built from one closure.
    depth: usize,
    /// Objects the root reached when the bundle was last built from one
    /// closure; [`CheckpointObjectBundle::trimmed`] waits for twice as many.
    trim_floor: usize,
}

/// Layers a bundle may chain before a trim is due whatever its size. Dropping
/// the chain releases one layer per frame, so an unbounded chain is an
/// unbounded stack.
const MAX_LAYER_DEPTH: usize = 256;

#[derive(Eq, PartialEq)]
struct ObjectLayer {
    prior: Option<Arc<Self>>,
    objects: Arc<[RuntimeObject]>,
}

/// Persistent Patricia index used only to prove CAS membership/reversion
/// without scanning checkpoint ancestry. Its depth is bounded by the fixed
/// 296-bit address width, so each supplied object adds constant-bounded metadata.
#[derive(Clone)]
enum CasNode {
    Leaf(RuntimeObject),
    Branch {
        differing_bit: u16,
        zero: Arc<Self>,
        one: Arc<Self>,
    },
}

impl CasNode {
    fn get(mut node: Option<&Self>, address: RuntimeObjectAddress) -> Option<&RuntimeObject> {
        while let Some(current) = node {
            match current {
                Self::Leaf(object) => return (object.address == address).then_some(object),
                Self::Branch {
                    differing_bit,
                    zero,
                    one,
                } => {
                    node = Some(if address_bit(address, *differing_bit) {
                        one
                    } else {
                        zero
                    });
                }
            }
        }
        None
    }

    fn insert(
        node: Option<&Arc<Self>>,
        object: RuntimeObject,
    ) -> Result<(Arc<Self>, bool, usize), CheckpointBundleError> {
        let Some(node) = node else {
            return Ok((Arc::new(Self::Leaf(object)), true, 1));
        };
        let existing = Self::leaf_for(node, object.address);
        if existing.address == object.address {
            if existing != &object {
                return Err(CheckpointBundleError::AddressConflict);
            }
            return Ok((node.clone(), false, 0));
        }
        let differing_bit = first_differing_address_bit(existing.address, object.address)
            .ok_or(CheckpointBundleError::AddressConflict)?;
        let (root, nodes_created) = Self::insert_at(node, differing_bit, object);
        Ok((root, true, nodes_created))
    }

    fn leaf_for(mut node: &Self, address: RuntimeObjectAddress) -> &RuntimeObject {
        loop {
            match node {
                Self::Leaf(object) => return object,
                Self::Branch {
                    differing_bit,
                    zero,
                    one,
                } => {
                    node = if address_bit(address, *differing_bit) {
                        one
                    } else {
                        zero
                    };
                }
            }
        }
    }

    fn insert_at(
        node: &Arc<Self>,
        differing_bit: u16,
        object: RuntimeObject,
    ) -> (Arc<Self>, usize) {
        if let Self::Branch {
            differing_bit: current_bit,
            zero,
            one,
        } = node.as_ref()
            && *current_bit < differing_bit
        {
            return if address_bit(object.address, *current_bit) {
                let (new_one, nodes_created) = Self::insert_at(one, differing_bit, object);
                (
                    Arc::new(Self::Branch {
                        differing_bit: *current_bit,
                        zero: zero.clone(),
                        one: new_one,
                    }),
                    nodes_created.saturating_add(1),
                )
            } else {
                let (new_zero, nodes_created) = Self::insert_at(zero, differing_bit, object);
                (
                    Arc::new(Self::Branch {
                        differing_bit: *current_bit,
                        zero: new_zero,
                        one: one.clone(),
                    }),
                    nodes_created.saturating_add(1),
                )
            };
        }
        let existing = node.clone();
        let object_address = object.address;
        let added = Arc::new(Self::Leaf(object));
        let (zero, one) = if address_bit(object_address, differing_bit) {
            (existing, added)
        } else {
            (added, existing)
        };
        (
            Arc::new(Self::Branch {
                differing_bit,
                zero,
                one,
            }),
            2,
        )
    }
}

fn first_differing_address_bit(
    left: RuntimeObjectAddress,
    right: RuntimeObjectAddress,
) -> Option<u16> {
    (0..296_u16).find(|bit| address_bit(left, *bit) != address_bit(right, *bit))
}

fn address_bit(address: RuntimeObjectAddress, bit: u16) -> bool {
    let byte_index = usize::from(bit / 8);
    let byte = match byte_index {
        0 => address.kind as u8,
        1..=4 => address.codec_revision.to_be_bytes()[byte_index.saturating_sub(1)],
        5..=36 => address.digest.bytes()[byte_index.saturating_sub(5)],
        _ => 0,
    };
    byte & (0x80_u8 >> u32::from(bit % 8)) != 0
}

#[derive(Debug, Eq, PartialEq)]
struct BundleReuse {
    canonical: RuntimeObjectAddress,
    harness: RuntimeObjectAddress,
    provider_configuration: RuntimeObjectAddress,
    prefix: RuntimeObjectAddress,
    history_head: Option<RuntimeObjectAddress>,
    working: RuntimeObjectAddress,
    context: RuntimeObjectAddress,
    binding: RuntimeObjectAddress,
    tools: RuntimeObjectAddress,
    command_count: usize,
    command_head: Option<RuntimeObjectAddress>,
    operation_count: usize,
    operation_head: Option<RuntimeObjectAddress>,
    ledger: RuntimeObjectAddress,
    family: Option<RuntimeObjectAddress>,
}

impl fmt::Debug for CheckpointObjectBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CheckpointObjectBundle")
            .field("checkpoint_id", &self.checkpoint_id)
            .field("checkpoint_integrity", &self.checkpoint_integrity)
            .field("base_root", &self.base_root)
            .field("root", &self.root)
            .field("supplied_object_count", &self.objects.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl PartialEq for CheckpointObjectBundle {
    fn eq(&self, other: &Self) -> bool {
        self.checkpoint_id == other.checkpoint_id
            && self.checkpoint_integrity == other.checkpoint_integrity
            && self.base_root == other.base_root
            && self.root == other.root
            && self.objects == other.objects
            && self.stats == other.stats
    }
}

impl CheckpointObjectBundle {
    /// Reassembles a complete reachable object set loaded by a storage backend.
    ///
    /// The supplied objects must already be the exact canonical sorted closure;
    /// missing, extra, corrupt, or reordered objects are rejected immediately.
    ///
    /// # Errors
    ///
    /// Returns a typed error when any address, object, edge, semantic value, or
    /// semantic checkpoint identity fails exact validation.
    pub fn from_store(
        checkpoint_id: CheckpointId,
        checkpoint_integrity: Digest,
        root: RuntimeObjectAddress,
        objects: Vec<RuntimeObject>,
    ) -> Result<Self, CheckpointBundleError> {
        let retained_bytes = objects
            .iter()
            .map(RuntimeObject::retained_bytes)
            .fold(0_usize, usize::saturating_add);
        let objects: Arc<[RuntimeObject]> = objects.into();
        let (cas, _, index_nodes) = cas_insert_all(None, &objects)?;
        let bundle = Self {
            checkpoint_id,
            checkpoint_integrity,
            base_root: None,
            root,
            stats: CheckpointBundleStats {
                object_count: objects.len(),
                retained_bytes,
                new_object_count: objects.len(),
                new_retained_bytes: retained_bytes,
                new_index_node_count: index_nodes,
                ..CheckpointBundleStats::default()
            },
            depth: 1,
            trim_floor: objects.len(),
            objects: objects.clone(),
            layers: Arc::new(ObjectLayer {
                prior: None,
                objects,
            }),
            cas,
            checkpoint: None,
            reuse: None,
        };
        let mut bundle = bundle;
        let checkpoint = RuntimeCheckpointCodec::materialize(&bundle)?;
        bundle.checkpoint = Some(Arc::new(checkpoint));
        bundle.reuse = Some(Arc::new(reuse_from_bundle(&bundle)?));
        Ok(bundle)
    }

    /// Returns the checkpoint identity bound by the root.
    #[must_use]
    pub const fn checkpoint_id(&self) -> CheckpointId {
        self.checkpoint_id
    }

    /// Returns the compositional semantic checkpoint integrity bound by the root.
    #[must_use]
    pub const fn checkpoint_integrity(&self) -> Digest {
        self.checkpoint_integrity
    }

    /// Returns the exact prior checkpoint root this publication extends.
    ///
    /// Genesis and cold full-store reconstruction have no base root.
    #[must_use]
    pub const fn base_root(&self) -> Option<RuntimeObjectAddress> {
        self.base_root
    }

    /// Returns the small canonical checkpoint-root address.
    #[must_use]
    pub const fn root(&self) -> RuntimeObjectAddress {
        self.root
    }

    /// Returns the exact canonical sorted new CAS objects for this publication.
    ///
    /// Genesis returns the full reachable closure. Successor publications return
    /// only objects absent from the validated base publication ancestry.
    #[must_use]
    pub fn objects(&self) -> &[RuntimeObject] {
        &self.objects
    }

    /// Explicit alias for [`Self::objects`] used by storage backends.
    #[must_use]
    pub fn supplied_objects(&self) -> &[RuntimeObject] {
        &self.objects
    }

    /// Returns sharing and retained-growth counters relative to the supplied prior bundle.
    #[must_use]
    pub const fn stats(&self) -> CheckpointBundleStats {
        self.stats
    }

    /// Returns the sealed checkpoint validated when this publication was built or loaded.
    ///
    /// Storage commit paths use this value while validating only the supplied
    /// object delta against [`Self::base_root`]. Cold load still calls the full
    /// graph codec before this seal is installed.
    ///
    /// # Errors
    ///
    /// Returns an error only for an internally unsealed bundle under construction.
    pub fn validated_checkpoint(&self) -> Result<Arc<RuntimeCheckpoint>, CheckpointBundleError> {
        self.checkpoint
            .clone()
            .ok_or(CheckpointBundleError::SemanticDisagreement)
    }

    fn lookup(&self, address: RuntimeObjectAddress) -> Option<&RuntimeObject> {
        CasNode::get(self.cas.as_deref(), address)
    }

    /// Forgets the objects this root no longer reaches, once they are due.
    ///
    /// [`RuntimeCheckpointCodec::advance`] dedupes against every object the
    /// bundle has seen, so without this an owner keeps every superseded
    /// object for its whole life. A trim is due when the chain is
    /// `MAX_LAYER_DEPTH` layers deep or holds twice what the root reached
    /// at the last trim, which keeps its cost proportional to what was
    /// published since. Call it only on a bundle whose commit was
    /// acknowledged: before that, its supplied objects are the commit's delta.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the root reaches an object the bundle lacks.
    pub fn trimmed(&self) -> Result<Self, CheckpointBundleError> {
        if self.depth < MAX_LAYER_DEPTH
            && self.stats.object_count < self.trim_floor.saturating_mul(2)
        {
            return Ok(self.clone());
        }
        self.current_root_closure()
    }

    /// Unconditionally forgets ancestral CAS membership before a store sweep.
    /// Unlike amortized trimming, this must run even below the size/depth
    /// thresholds: an old address may be reconstructed after pruning removed it.
    /// The returned closure is loaded/published state, never a pending
    /// publication delta. Retain the original delta until acknowledgement.
    ///
    /// # Errors
    /// Returns a typed error if the root closure is missing objects or exceeds bounds.
    #[doc(hidden)]
    pub fn current_root_closure(&self) -> Result<Self, CheckpointBundleError> {
        let mut closure = BTreeMap::new();
        let mut pending = vec![self.root];
        while let Some(address) = pending.pop() {
            if closure.contains_key(&address) {
                continue;
            }
            let object = self
                .lookup(address)
                .ok_or(CheckpointBundleError::MissingObject)?;
            pending.extend_from_slice(object.children());
            closure.insert(address, object.clone());
        }
        let objects: Arc<[RuntimeObject]> = closure.into_values().collect();
        let retained_bytes = objects
            .iter()
            .map(RuntimeObject::retained_bytes)
            .fold(0_usize, usize::saturating_add);
        if objects.len() > MAX_CHECKPOINT_OBJECTS || retained_bytes > MAX_CHECKPOINT_OBJECT_BYTES {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        let (cas, _, index_nodes) = cas_insert_all(None, &objects)?;
        Ok(Self {
            checkpoint_id: self.checkpoint_id,
            checkpoint_integrity: self.checkpoint_integrity,
            base_root: None,
            root: self.root,
            stats: CheckpointBundleStats {
                object_count: objects.len(),
                retained_bytes,
                new_object_count: objects.len(),
                new_retained_bytes: retained_bytes,
                new_index_node_count: index_nodes,
                ..CheckpointBundleStats::default()
            },
            depth: 1,
            trim_floor: objects.len(),
            layers: Arc::new(ObjectLayer {
                prior: None,
                objects: objects.clone(),
            }),
            objects,
            cas,
            checkpoint: self.checkpoint.clone(),
            reuse: self.reuse.clone(),
        })
    }
}

fn cas_insert_all(
    mut root: Option<Arc<CasNode>>,
    objects: &[RuntimeObject],
) -> Result<(Option<Arc<CasNode>>, usize, usize), CheckpointBundleError> {
    let mut inserted = 0_usize;
    let mut nodes_created = 0_usize;
    for object in objects {
        let (next, was_inserted, object_nodes_created) =
            CasNode::insert(root.as_ref(), object.clone())?;
        root = Some(next);
        inserted = inserted.saturating_add(usize::from(was_inserted));
        nodes_created = nodes_created.saturating_add(object_nodes_created);
    }
    Ok((root, inserted, nodes_created))
}

/// Sole agent-owned persisted codec for the closed checkpoint object graph.
#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeCheckpointCodec;

/// Read-only diagnostic metadata, not a materialized or recovery-authorizing checkpoint.
#[derive(Clone, Debug)]
pub struct RuntimeCheckpointInspection {
    /// Exact topology bound by the manifest metadata address.
    pub topology: crate::manifest::RuntimeTopology,
    /// Recorded runtime lifecycle.
    pub lifecycle: crate::manifest::RuntimeLifecycle,
    /// Recorded durability.
    pub durability: crate::manifest::RuntimeDurability,
    /// Operation ids and canonical request identities named by recovery obligations.
    pub requests: Vec<(u64, Digest)>,
}

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

/// Typed fail-closed bundle construction or validation error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum CheckpointBundleError {
    /// An embedded semantic value failed its owning canonical codec.
    #[error("checkpoint object semantic value is invalid: {0}")]
    Manifest(#[from] RuntimeManifestError),
    /// A closed object-kind tag is unknown.
    #[error("checkpoint object kind tag {0} is unknown")]
    UnknownObjectKind(u8),
    /// An object or address uses a codec revision this codec does not implement.
    #[error("checkpoint object revision is unsupported")]
    UnsupportedRevision,
    /// An address does not match its exact kind, revision, and canonical bytes.
    #[error("checkpoint object content address is invalid")]
    InvalidAddress,
    /// Two values attempted to occupy one content address.
    #[error("checkpoint object address collision or disagreement")]
    AddressConflict,
    /// A required root or child object is absent.
    #[error("checkpoint object graph is missing a required object")]
    MissingObject,
    /// The supplied set contains an object unreachable from the sole root.
    #[error("checkpoint object graph contains an extra object")]
    ExtraObject,
    /// A child occupied the wrong closed semantic role.
    #[error("checkpoint object child has the wrong kind")]
    WrongKind,
    /// Canonical bytes or their ordered child mirror are not the sole encoding.
    #[error("checkpoint object encoding is noncanonical")]
    NonCanonical,
    /// A persistent vector count, order, or topology is invalid.
    #[error("checkpoint object graph topology is invalid")]
    InvalidTopology,
    /// A cycle was observed in the required object graph.
    #[error("checkpoint object graph contains a cycle")]
    Cycle,
    /// Object count, depth, or aggregate canonical bytes exceed fixed bounds.
    #[error("checkpoint object graph exceeds a fixed bound")]
    BoundExceeded,
    /// The root identities disagree with the materialized semantic checkpoint.
    #[error("checkpoint object graph disagrees with the semantic checkpoint")]
    SemanticDisagreement,
    /// The requested ordinary advance is not an exact append over the supplied base.
    #[error("checkpoint object incremental base does not match the semantic append")]
    IncrementalBaseMismatch,
    /// A custom bundle object is truncated or malformed.
    #[error("checkpoint object encoding is malformed")]
    Malformed,
}

struct BundleBuilder {
    objects: BTreeMap<RuntimeObjectAddress, RuntimeObject>,
    visited: usize,
}

struct BuiltCheckpoint {
    root: RuntimeObjectAddress,
    reuse: BundleReuse,
}

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
    fn retain_candidate_closure(
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

    fn checkpoint(
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
            },
        })
    }

    fn advance(
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

    fn working(
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

    fn segment(
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

    fn component(
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

    fn operation_tree_upsert(
        &mut self,
        root: Option<RuntimeObjectAddress>,
        key: u64,
        record: RuntimeObjectAddress,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<(RuntimeObjectAddress, bool), CheckpointBundleError> {
        let Some(root) = root else {
            return self
                .operation_tree_node(key, None, record, None, prior)
                .map(|address| (address, true));
        };
        let node = self.operation_tree_spec(root, prior)?;
        match key.cmp(&node.key) {
            Ordering::Less => {
                let (left, inserted) = self.operation_tree_upsert(node.left, key, record, prior)?;
                let address = self.operation_tree_balance(
                    node.key,
                    Some(left),
                    node.record,
                    node.right,
                    prior,
                )?;
                self.objects.remove(&root);
                Ok((address, inserted))
            }
            Ordering::Greater => {
                let (right, inserted) =
                    self.operation_tree_upsert(node.right, key, record, prior)?;
                let address = self.operation_tree_balance(
                    node.key,
                    node.left,
                    node.record,
                    Some(right),
                    prior,
                )?;
                self.objects.remove(&root);
                Ok((address, inserted))
            }
            Ordering::Equal => self
                .operation_tree_node(key, node.left, record, node.right, prior)
                .map(|address| (address, false)),
        }
    }

    fn operation_tree_balance(
        &mut self,
        key: u64,
        left: Option<RuntimeObjectAddress>,
        record: RuntimeObjectAddress,
        right: Option<RuntimeObjectAddress>,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let left_height = self.operation_tree_height(left, prior)?;
        let right_height = self.operation_tree_height(right, prior)?;
        let skew = i16::from(left_height) - i16::from(right_height);
        if skew > 1 {
            let pivot_address = left.ok_or(CheckpointBundleError::InvalidTopology)?;
            let pivot = self.operation_tree_spec(pivot_address, prior)?;
            if self.operation_tree_height(pivot.left, prior)?
                >= self.operation_tree_height(pivot.right, prior)?
            {
                let new_right = self.operation_tree_node(key, pivot.right, record, right, prior)?;
                self.objects.remove(&pivot_address);
                return self.operation_tree_node(
                    pivot.key,
                    pivot.left,
                    pivot.record,
                    Some(new_right),
                    prior,
                );
            }
            let middle_address = pivot.right.ok_or(CheckpointBundleError::InvalidTopology)?;
            let middle = self.operation_tree_spec(middle_address, prior)?;
            let new_left =
                self.operation_tree_node(pivot.key, pivot.left, pivot.record, middle.left, prior)?;
            let new_right = self.operation_tree_node(key, middle.right, record, right, prior)?;
            self.objects.remove(&pivot_address);
            self.objects.remove(&middle_address);
            return self.operation_tree_node(
                middle.key,
                Some(new_left),
                middle.record,
                Some(new_right),
                prior,
            );
        }
        if skew < -1 {
            let pivot_address = right.ok_or(CheckpointBundleError::InvalidTopology)?;
            let pivot = self.operation_tree_spec(pivot_address, prior)?;
            if self.operation_tree_height(pivot.right, prior)?
                >= self.operation_tree_height(pivot.left, prior)?
            {
                let new_left = self.operation_tree_node(key, left, record, pivot.left, prior)?;
                self.objects.remove(&pivot_address);
                return self.operation_tree_node(
                    pivot.key,
                    Some(new_left),
                    pivot.record,
                    pivot.right,
                    prior,
                );
            }
            let middle_address = pivot.left.ok_or(CheckpointBundleError::InvalidTopology)?;
            let middle = self.operation_tree_spec(middle_address, prior)?;
            let new_left = self.operation_tree_node(key, left, record, middle.left, prior)?;
            let new_right = self.operation_tree_node(
                pivot.key,
                middle.right,
                pivot.record,
                pivot.right,
                prior,
            )?;
            self.objects.remove(&pivot_address);
            self.objects.remove(&middle_address);
            return self.operation_tree_node(
                middle.key,
                Some(new_left),
                middle.record,
                Some(new_right),
                prior,
            );
        }
        self.operation_tree_node(key, left, record, right, prior)
    }

    fn operation_tree_node(
        &mut self,
        key: u64,
        left: Option<RuntimeObjectAddress>,
        record: RuntimeObjectAddress,
        right: Option<RuntimeObjectAddress>,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let left_spec = left
            .map(|address| self.operation_tree_spec(address, prior))
            .transpose()?;
        let right_spec = right
            .map(|address| self.operation_tree_spec(address, prior))
            .transpose()?;
        let count = left_spec
            .map_or(0, |node| node.count)
            .saturating_add(right_spec.map_or(0, |node| node.count))
            .saturating_add(1);
        let height = left_spec
            .map_or(0, |node| node.height)
            .max(right_spec.map_or(0, |node| node.height))
            .saturating_add(1);
        let mut writer = BundleWriter::default();
        writer.u64(key);
        writer.count(count)?;
        writer.u8(height);
        writer.optional_address(left);
        writer.address(record);
        writer.optional_address(right);
        let mut children = Vec::with_capacity(3);
        children.extend(left);
        children.push(record);
        children.extend(right);
        self.insert(RuntimeObjectKind::OperationNode, writer.finish(), children)
    }

    fn operation_tree_height(
        &self,
        address: Option<RuntimeObjectAddress>,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<u8, CheckpointBundleError> {
        address
            .map(|address| {
                self.operation_tree_spec(address, prior)
                    .map(|node| node.height)
            })
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn operation_tree_spec(
        &self,
        address: RuntimeObjectAddress,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<OperationNodeSpec, CheckpointBundleError> {
        let object = self
            .objects
            .get(&address)
            .or_else(|| prior.and_then(|bundle| bundle.lookup(address)))
            .ok_or(CheckpointBundleError::MissingObject)?;
        if object.address.kind != RuntimeObjectKind::OperationNode {
            return Err(CheckpointBundleError::WrongKind);
        }
        parse_operation_node(&object.canonical_bytes)
    }

    fn insert(
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

#[cfg(test)]
thread_local! {
    static OBJECT_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ADDRESS_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEGMENT_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEGMENT_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn object_address(
    kind: RuntimeObjectKind,
    canonical_bytes: &[u8],
) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
    #[cfg(test)]
    {
        OBJECT_HASHES.update(|count| count + 1);
        if kind == RuntimeObjectKind::HistorySegment {
            SEGMENT_HASHES.update(|count| count + 1);
        }
    }
    let codec_revision = RUNTIME_FORMAT_EPOCH;
    let digest = Digest::sha256(
        DigestDomain::Blob,
        codec_revision,
        &[
            OBJECT_ADDRESS_DOMAIN,
            &[kind as u8],
            &codec_revision.to_be_bytes(),
            canonical_bytes,
        ],
    )
    .map_err(|_| CheckpointBundleError::InvalidAddress)?;
    Ok(RuntimeObjectAddress {
        kind,
        codec_revision,
        digest,
    })
}

fn canonical_child_specs(
    kind: RuntimeObjectKind,
    canonical_bytes: &[u8],
) -> Result<Vec<(RuntimeObjectAddress, RuntimeObjectKind)>, CheckpointBundleError> {
    let children = match kind {
        RuntimeObjectKind::CheckpointRoot => {
            let (_, _, manifest) = parse_checkpoint_root(canonical_bytes)?;
            vec![(manifest, RuntimeObjectKind::ManifestRoot)]
        }
        RuntimeObjectKind::ManifestRoot => {
            let root = parse_manifest_root(canonical_bytes)?;
            let mut children = vec![
                (root.metadata, RuntimeObjectKind::ManifestMetadata),
                (root.harness, RuntimeObjectKind::ReconstructiveComponent),
                (
                    root.provider_configuration,
                    RuntimeObjectKind::ProviderConfiguration,
                ),
                (root.canonical, RuntimeObjectKind::CanonicalState),
                (root.binding, RuntimeObjectKind::ModelRouteBinding),
                (root.tools, RuntimeObjectKind::ToolRuntime),
                (root.ledger, RuntimeObjectKind::OperationLedger),
            ];
            children.extend(
                root.family
                    .map(|address| (address, RuntimeObjectKind::FamilyArtifact)),
            );
            children
        }
        RuntimeObjectKind::ManifestMetadata => {
            canonical_metadata(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::StablePrefix => {
            canonical_stable_prefix(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::HistorySegment => {
            canonical_history_segment(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::HistoryNode => {
            let node = parse_sequence_node(canonical_bytes)?;
            let mut children = Vec::with_capacity(2);
            children.extend(
                node.previous
                    .map(|address| (address, RuntimeObjectKind::HistoryNode)),
            );
            children.push((node.value, RuntimeObjectKind::HistorySegment));
            children
        }
        RuntimeObjectKind::WorkingProjection => parse_working(canonical_bytes)?
            .segments
            .into_iter()
            .map(|address| (address, RuntimeObjectKind::HistorySegment))
            .collect(),
        RuntimeObjectKind::ReconstructiveComponent => {
            canonical_component(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::ContextState => {
            RuntimeContextState::decode(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::ProviderConfiguration => {
            canonical_provider_configuration(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::CanonicalState => {
            let canonical = parse_canonical(canonical_bytes)?;
            let mut children = vec![(canonical.prefix, RuntimeObjectKind::StablePrefix)];
            children.extend(
                canonical
                    .history_head
                    .map(|address| (address, RuntimeObjectKind::HistoryNode)),
            );
            children.extend([
                (canonical.working, RuntimeObjectKind::WorkingProjection),
                (canonical.context, RuntimeObjectKind::ContextState),
            ]);
            children
        }
        RuntimeObjectKind::ModelRouteBinding => {
            let binding = canonical_binding(canonical_bytes)?;
            [binding.descriptor(), binding.admission().evidence()]
                .into_iter()
                .map(|component| {
                    component_address(component)
                        .map(|address| (address, RuntimeObjectKind::ReconstructiveComponent))
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        RuntimeObjectKind::ToolRuntime => {
            let tools = canonical_tools(canonical_bytes)?;
            [
                tools.registry(),
                tools.exposure(),
                tools.environment(),
                tools.effect_policy(),
            ]
            .into_iter()
            .map(|component| {
                component_address(component)
                    .map(|address| (address, RuntimeObjectKind::ReconstructiveComponent))
            })
            .collect::<Result<Vec<_>, _>>()?
        }
        RuntimeObjectKind::CommandRecord => {
            let record = canonical_command(canonical_bytes)?;
            vec![(
                component_address(record.command_component())?,
                RuntimeObjectKind::ReconstructiveComponent,
            )]
        }
        RuntimeObjectKind::CommandNode => {
            let node = parse_sequence_node(canonical_bytes)?;
            let mut children = Vec::with_capacity(2);
            children.extend(
                node.previous
                    .map(|address| (address, RuntimeObjectKind::CommandNode)),
            );
            children.push((node.value, RuntimeObjectKind::CommandRecord));
            children
        }
        RuntimeObjectKind::OperationRecord => canonical_operation_wire(canonical_bytes)?
            .component_digests()
            .into_iter()
            .map(|digest| {
                component_object_address(digest)
                    .map(|address| (address, RuntimeObjectKind::ReconstructiveComponent))
            })
            .collect::<Result<Vec<_>, _>>()?,
        RuntimeObjectKind::OperationNode => {
            let node = parse_operation_node(canonical_bytes)?;
            let mut children = Vec::with_capacity(3);
            children.extend(
                node.left
                    .map(|address| (address, RuntimeObjectKind::OperationNode)),
            );
            children.push((node.record, RuntimeObjectKind::OperationRecord));
            children.extend(
                node.right
                    .map(|address| (address, RuntimeObjectKind::OperationNode)),
            );
            children
        }
        RuntimeObjectKind::OperationLedger => {
            let ledger = parse_ledger(canonical_bytes)?;
            let mut children = Vec::with_capacity(2);
            children.extend(
                ledger
                    .command_head
                    .map(|address| (address, RuntimeObjectKind::CommandNode)),
            );
            children.extend(
                ledger
                    .operation_head
                    .map(|address| (address, RuntimeObjectKind::OperationNode)),
            );
            children
        }
        RuntimeObjectKind::FamilyArtifact => {
            let artifact = canonical_family_artifact(canonical_bytes)?;
            vec![(
                component_address(artifact.binding_component())?,
                RuntimeObjectKind::ReconstructiveComponent,
            )]
        }
    };
    Ok(children)
}

struct ObjectIndex<'a> {
    objects: BTreeMap<RuntimeObjectAddress, &'a RuntimeObject>,
    decoded: BTreeMap<RuntimeObjectAddress, HistorySegment>,
}

impl<'a> ObjectIndex<'a> {
    fn validate(bundle: &'a CheckpointObjectBundle) -> Result<Self, CheckpointBundleError> {
        if bundle.objects.is_empty() || bundle.objects.len() > MAX_CHECKPOINT_OBJECTS {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        let mut total = 0_usize;
        let mut objects = BTreeMap::new();
        let mut layer = Some(bundle.layers.as_ref());
        while let Some(current) = layer {
            if current
                .objects
                .windows(2)
                .any(|pair| pair[0].address >= pair[1].address)
            {
                return Err(CheckpointBundleError::NonCanonical);
            }
            for object in current.objects.iter() {
                total = total.saturating_add(object.retained_bytes());
                if total > MAX_CHECKPOINT_OBJECT_BYTES {
                    return Err(CheckpointBundleError::BoundExceeded);
                }
                if object.address.codec_revision != RUNTIME_FORMAT_EPOCH
                    || object.address.digest.algorithm() != DigestAlgorithm::Sha256
                    || object.address.digest.domain() != DigestDomain::Blob
                    || object.address.digest.version() != RUNTIME_FORMAT_EPOCH
                {
                    return Err(CheckpointBundleError::UnsupportedRevision);
                }
                // Both fields are construction-only: constructors either hash
                // these bytes or receive the builder's freshly computed address.
                // Pointer equality detects test-only byte replacement, not a memo.
                if !Arc::ptr_eq(&object.addressed_bytes, &object.canonical_bytes) {
                    return Err(CheckpointBundleError::InvalidAddress);
                }
                if objects.insert(object.address, object).is_some() {
                    return Err(CheckpointBundleError::NonCanonical);
                }
            }
            layer = current.prior.as_deref();
        }
        if objects.len() > MAX_CHECKPOINT_OBJECTS {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        let decoded = objects
            .iter()
            .filter(|(address, _)| address.kind == RuntimeObjectKind::HistorySegment)
            .map(|(address, object)| {
                let segment = match &object.decoded_segment {
                    Some(segment) => segment.clone(),
                    None => canonical_history_segment(&object.canonical_bytes)?,
                };
                Ok((*address, segment))
            })
            .collect::<Result<_, CheckpointBundleError>>()?;
        let index = Self { objects, decoded };
        index.object(bundle.root, RuntimeObjectKind::CheckpointRoot)?;
        for object in bundle.objects.iter() {
            let expected = index.expected_children(object)?;
            if expected.as_slice() != object.children() {
                return Err(CheckpointBundleError::NonCanonical);
            }
            for child in &expected {
                index
                    .objects
                    .get(child)
                    .ok_or(CheckpointBundleError::MissingObject)?;
            }
        }
        let reachable = index.validate_closed_acyclic(bundle.root)?;
        if bundle
            .objects
            .iter()
            .any(|object| !reachable.contains(&object.address))
        {
            return Err(CheckpointBundleError::ExtraObject);
        }
        Ok(index)
    }

    fn object(
        &self,
        address: RuntimeObjectAddress,
        kind: RuntimeObjectKind,
    ) -> Result<&'a RuntimeObject, CheckpointBundleError> {
        let object = self
            .objects
            .get(&address)
            .copied()
            .ok_or(CheckpointBundleError::MissingObject)?;
        if object.address.kind != kind {
            return Err(CheckpointBundleError::WrongKind);
        }
        Ok(object)
    }

    fn expected_children(
        &self,
        object: &RuntimeObject,
    ) -> Result<Vec<RuntimeObjectAddress>, CheckpointBundleError> {
        if self.decoded.contains_key(&object.address) {
            return Ok(Vec::new());
        }
        let child_specs = canonical_child_specs(object.address.kind, &object.canonical_bytes)?;
        for (address, kind) in &child_specs {
            self.object(*address, *kind)?;
        }
        Ok(child_specs
            .into_iter()
            .map(|(address, _)| address)
            .collect())
    }

    fn validate_closed_acyclic(
        &self,
        root: RuntimeObjectAddress,
    ) -> Result<BTreeSet<RuntimeObjectAddress>, CheckpointBundleError> {
        let mut visiting = BTreeSet::new();
        let mut done = BTreeSet::new();
        let mut stack = vec![(root, false)];
        while let Some((address, expanded)) = stack.pop() {
            if expanded {
                visiting.remove(&address);
                done.insert(address);
                continue;
            }
            if done.contains(&address) {
                continue;
            }
            if !visiting.insert(address) {
                return Err(CheckpointBundleError::Cycle);
            }
            if stack.len() > MAX_CHECKPOINT_OBJECTS {
                return Err(CheckpointBundleError::BoundExceeded);
            }
            stack.push((address, true));
            let object = self
                .objects
                .get(&address)
                .ok_or(CheckpointBundleError::MissingObject)?;
            for child in object.children.iter().rev() {
                stack.push((*child, false));
            }
        }
        Ok(done)
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
    RuntimeOperationLedger::new(commands, operations).map_err(Into::into)
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

fn materialize_operation_tree<'a>(
    index: &'a ObjectIndex<'a>,
    count: usize,
    root: Option<RuntimeObjectAddress>,
) -> Result<Vec<&'a RuntimeObject>, CheckpointBundleError> {
    if (count == 0) != root.is_none() {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    let mut summaries = BTreeMap::<RuntimeObjectAddress, (usize, u8, u64, u64)>::new();
    let mut stack = vec![(root, false)];
    while let Some((address, expanded)) = stack.pop() {
        let object = index.object(address, RuntimeObjectKind::OperationNode)?;
        let node = parse_operation_node(&object.canonical_bytes)?;
        if !expanded {
            stack.push((address, true));
            if let Some(right) = node.right {
                stack.push((right, false));
            }
            if let Some(left) = node.left {
                stack.push((left, false));
            }
            continue;
        }
        let left = node
            .left
            .map(|child| {
                summaries
                    .get(&child)
                    .copied()
                    .ok_or(CheckpointBundleError::InvalidTopology)
            })
            .transpose()?;
        let right = node
            .right
            .map(|child| {
                summaries
                    .get(&child)
                    .copied()
                    .ok_or(CheckpointBundleError::InvalidTopology)
            })
            .transpose()?;
        if left.is_some_and(|(_, _, _, maximum)| maximum >= node.key)
            || right.is_some_and(|(_, _, minimum, _)| minimum <= node.key)
        {
            return Err(CheckpointBundleError::InvalidTopology);
        }
        let left_count = left.map_or(0, |summary| summary.0);
        let right_count = right.map_or(0, |summary| summary.0);
        let left_height = left.map_or(0, |summary| summary.1);
        let right_height = right.map_or(0, |summary| summary.1);
        let expected_count = left_count.saturating_add(right_count).saturating_add(1);
        let expected_height = left_height.max(right_height).saturating_add(1);
        if node.count != expected_count
            || node.height != expected_height
            || left_height.abs_diff(right_height) > 1
        {
            return Err(CheckpointBundleError::InvalidTopology);
        }
        let record = index.object(node.record, RuntimeObjectKind::OperationRecord)?;
        if canonical_operation_wire(&record.canonical_bytes)?.operation_id() != node.key {
            return Err(CheckpointBundleError::InvalidTopology);
        }
        summaries.insert(
            address,
            (
                expected_count,
                expected_height,
                left.map_or(node.key, |summary| summary.2),
                right.map_or(node.key, |summary| summary.3),
            ),
        );
    }
    if summaries.get(&root).map(|summary| summary.0) != Some(count) {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    let mut records = Vec::with_capacity(count);
    let mut nodes = Vec::new();
    let mut current = Some(root);
    while current.is_some() || !nodes.is_empty() {
        while let Some(address) = current {
            nodes.push(address);
            current = parse_operation_node(
                &index
                    .object(address, RuntimeObjectKind::OperationNode)?
                    .canonical_bytes,
            )?
            .left;
        }
        let address = nodes.pop().ok_or(CheckpointBundleError::InvalidTopology)?;
        let node = parse_operation_node(
            &index
                .object(address, RuntimeObjectKind::OperationNode)?
                .canonical_bytes,
        )?;
        records.push(index.object(node.record, RuntimeObjectKind::OperationRecord)?);
        current = node.right;
    }
    Ok(records)
}

fn component_address(
    component: &ReconstructiveComponent,
) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
    object_address(
        RuntimeObjectKind::ReconstructiveComponent,
        &encode_bundle_component(component)?,
    )
}

/// Content digest of the reconstructive-component object holding `component`;
/// an operation record names its components by this digest.
pub(crate) fn component_object_digest(
    component: &ReconstructiveComponent,
) -> Result<Digest, RuntimeManifestError> {
    component_address(component)
        .map(|address| address.digest)
        .map_err(|error| match error {
            CheckpointBundleError::Manifest(error) => error,
            _ => RuntimeManifestError::InvalidDigest,
        })
}

fn component_object_address(digest: Digest) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
    RuntimeObjectAddress::from_store_parts(
        RuntimeObjectKind::ReconstructiveComponent,
        RUNTIME_FORMAT_EPOCH,
        digest,
    )
}

fn canonical_operation_wire(bytes: &[u8]) -> Result<OperationRecordWire, CheckpointBundleError> {
    let wire = OperationRecordWire::decode(bytes)?;
    if wire.encode()? != bytes {
        return Err(CheckpointBundleError::NonCanonical);
    }
    Ok(wire)
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

fn canonical_metadata(bytes: &[u8]) -> Result<(), CheckpointBundleError> {
    let metadata = decode_bundle_metadata(bytes)?;
    if encode_bundle_metadata_value(&metadata)? != bytes {
        return Err(CheckpointBundleError::NonCanonical);
    }
    Ok(())
}

macro_rules! canonical_value {
    ($name:ident, $ty:ty, $decode:ident, $encode:ident) => {
        fn $name(bytes: &[u8]) -> Result<$ty, CheckpointBundleError> {
            let value = $decode(bytes)?;
            if $encode(&value)? != bytes {
                return Err(CheckpointBundleError::NonCanonical);
            }
            Ok(value)
        }
    };
}

canonical_value!(
    canonical_stable_prefix,
    StableRequestPrefix,
    decode_bundle_stable_prefix,
    encode_bundle_stable_prefix
);
fn canonical_history_segment(bytes: &[u8]) -> Result<HistorySegment, CheckpointBundleError> {
    #[cfg(test)]
    SEGMENT_DECODES.update(|count| count + 1);
    Ok(decode_bundle_history_segment(bytes)?)
}
canonical_value!(
    canonical_component,
    ReconstructiveComponent,
    decode_bundle_component,
    encode_bundle_component
);
canonical_value!(
    canonical_provider_configuration,
    CompiledProviderConfiguration,
    decode_bundle_provider_configuration,
    encode_bundle_provider_configuration
);
canonical_value!(
    canonical_binding,
    ModelRouteBinding,
    decode_bundle_binding,
    encode_bundle_binding
);
canonical_value!(
    canonical_tools,
    ToolRuntimeManifest,
    decode_bundle_tools,
    encode_bundle_tools
);
canonical_value!(
    canonical_command,
    DurableCommandRecord,
    decode_bundle_command_record,
    encode_bundle_command_record
);
canonical_value!(
    canonical_family_artifact,
    FamilyArtifact,
    decode_bundle_family_artifact,
    encode_bundle_family_artifact
);

fn reuse_from_bundle(
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
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "negative bundle tests preserve exact mutation sites and diagnostics"
)]
pub(crate) mod tests;
